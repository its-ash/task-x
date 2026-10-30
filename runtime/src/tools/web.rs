//! Web access: fetch one http(s) URL as readable text or raw source.

use super::schema::{int, object};
use super::{Permission, ToolCtx, ToolDef, ToolError, ToolErrorKind, ToolResult, parse, tool};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::sync::LazyLock;
use std::time::Duration;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_CHARS: usize = 20_000;
const MAX_CHARS: usize = 50_000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_REDIRECTS: usize = 10;
const USER_AGENT: &str = concat!("task-x/", env!("CARGO_PKG_VERSION"), " (+web_fetch)");

/// Names that always mean "this machine or its private network".
const BLOCKED_NAMES: &[&str] =
    &["localhost", "local", "internal", "home.arpa", "metadata.google.internal", "host.docker.internal", "gateway.docker.internal"];

/// Whether an address belongs to the machine itself, its private network, or the link-local range
/// that holds cloud metadata endpoints (`169.254.169.254`).
fn blocked_ip(ip: IpAddr) -> bool {
    fn v4(ip: Ipv4Addr) -> bool {
        let o = ip.octets();
        ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_unspecified()
            || ip.is_broadcast()
            || ip.is_multicast()
            || o[0] == 0
            // 100.64.0.0/10 (carrier-grade NAT) and 198.18.0.0/15 (benchmarking).
            || (o[0] == 100 && (64..128).contains(&o[1]))
            || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
    }
    match ip {
        IpAddr::V4(ip) => v4(ip),
        IpAddr::V6(ip) => {
            let seg = ip.segments();
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                // fc00::/7 unique-local, fe80::/10 link-local.
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
                // ::ffff:a.b.c.d reaches an IPv4 address; judge it by the IPv4 rules.
                || ip.to_ipv4_mapped().is_some_and(v4)
        }
    }
}

fn private_target(host: &str) -> ToolError {
    ToolError::new(ToolErrorKind::Forbidden, format!("{host} is a local or private address; web_fetch reads only public sites"))
}

/// Rejects URLs that point at the machine running the runtime, its private network, or a cloud
/// metadata endpoint. Pages are untrusted input, so a poisoned page must not be able to steer the
/// agent at local services. Resolved addresses are checked too, so a public name pointing at
/// `127.0.0.1` is caught as well.
fn check_target(url: &url::Url) -> Result<(), ToolError> {
    let host = url.host_str().ok_or_else(|| ToolError::invalid("the URL needs a host"))?;
    let name = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if let Ok(ip) = name.parse::<IpAddr>() {
        return if blocked_ip(ip) { Err(private_target(host)) } else { Ok(()) };
    }
    if BLOCKED_NAMES.iter().any(|s| name == *s || name.ends_with(&format!(".{s}"))) {
        return Err(private_target(host));
    }
    let Some(port) = url.port_or_known_default() else { return Ok(()) };
    // A failed lookup is left to the request, which reports it in its own terms.
    if let Ok(addrs) = (name.as_str(), port).to_socket_addrs() {
        if addrs.map(|a| a.ip()).any(blocked_ip) {
            return Err(private_target(host));
        }
    }
    Ok(())
}

fn client() -> Result<reqwest::Client, ToolError> {
    let fail = |e: reqwest::Error| ToolError::new(ToolErrorKind::Failed, format!("fetch failed: {e}"));
    // Every hop is checked: a public URL must not redirect into a private one.
    let redirects = reqwest::redirect::Policy::custom(|a: reqwest::redirect::Attempt<'_>| {
        if a.previous().len() >= MAX_REDIRECTS {
            a.error("too many redirects")
        } else if check_target(a.url()).is_err() {
            a.error("a redirect to a local or private address is not allowed")
        } else {
            a.follow()
        }
    });
    reqwest::Client::builder().user_agent(USER_AGENT).timeout(FETCH_TIMEOUT).redirect(redirects).build().map_err(fail)
}

pub fn tools() -> Vec<ToolDef> {
    vec![tool(
        "web_fetch",
        "Fetch one http(s) URL (docs, API references, widget or embed pages). Returns the page as readable text, or the raw source with format \"source\" (use it to copy embed code, scripts, or JSON). Follows redirects.",
        Permission::Network,
        || {
            object(
                json!({
                    "url": { "type": "string", "description": "http:// or https:// URL" },
                    "format": { "type": "string", "enum": ["text", "source"], "description": "text (default): HTML reduced to readable text. source: the body as sent." },
                    "maxChars": int("Max characters returned (default 20000, max 50000)", 1),
                    "offset": int("Skip this many characters first, to read a long page in parts", 0),
                }),
                &["url"],
            )
        },
        web_fetch,
    ),
    tool(
        "web_search",
        "Search the web and get the top results (title, URL, snippet). Use it for current facts, error messages, library versions, or to find the docs page, then read a result with web_fetch.",
        Permission::Network,
        || {
            object(
                json!({
                    "query": { "type": "string", "description": "What to search for" },
                    "count": int("How many results (default 8, max 20)", 1),
                    "site": { "type": "string", "description": "Only results from this domain, e.g. docs.rs" },
                }),
                &["query"],
            )
        },
        web_search,
    )]
}

const SEARCH_URL: &str = "https://html.duckduckgo.com/html/";
const DEFAULT_RESULTS: usize = 8;
const MAX_RESULTS: usize = 20;
const MAX_QUERY_CHARS: usize = 400;

static RESULT_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).expect("regex"));
static RESULT_SNIPPET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</(?:a|td|div)>"#).expect("regex"));

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchInput {
    #[serde(alias = "q")]
    query: String,
    #[serde(default, alias = "limit", alias = "maxResults")]
    count: Option<usize>,
    #[serde(default)]
    site: Option<String>,
}

/// The real target of a DuckDuckGo result link (`//duckduckgo.com/l/?uddg=<url>`).
fn result_url(href: &str) -> Option<String> {
    let full = if href.starts_with("//") { format!("https:{href}") } else { href.to_owned() };
    let u = url::Url::parse(&full).ok()?;
    let target = if u.host_str().is_some_and(|h| h.ends_with("duckduckgo.com")) {
        u.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.into_owned())?
    } else {
        full
    };
    // Ads go through DuckDuckGo's own click tracking; they are left out.
    let t = url::Url::parse(&target).ok()?;
    (matches!(t.scheme(), "http" | "https") && !t.host_str().is_some_and(|h| h.ends_with("duckduckgo.com"))).then(|| t.to_string())
}

/// Search results from a DuckDuckGo HTML page.
fn parse_results(html: &str, max: usize) -> Vec<serde_json::Value> {
    let snippets: Vec<String> = RESULT_SNIPPET.captures_iter(html).map(|c| html_to_text(&c[1])).collect();
    let mut out = Vec::new();
    for (i, c) in RESULT_LINK.captures_iter(html).enumerate() {
        let Some(url) = result_url(&c[1]) else { continue };
        if out.iter().any(|r: &serde_json::Value| r["url"] == url.as_str()) {
            continue;
        }
        out.push(json!({ "title": html_to_text(&c[2]), "url": url, "snippet": snippets.get(i).cloned().unwrap_or_default() }));
        if out.len() >= max {
            break;
        }
    }
    out
}

fn web_search(ctx: &ToolCtx, input: serde_json::Value) -> ToolResult {
    let p: SearchInput = parse(input)?;
    let query = p.query.trim();
    if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
        return Err(ToolError::invalid(format!("query must be 1..={MAX_QUERY_CHARS} characters")));
    }
    let site = p.site.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if site.is_some_and(|s| !s.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))) {
        return Err(ToolError::invalid("site must be a domain name such as docs.rs"));
    }
    let q = site.map_or_else(|| query.to_owned(), |s| format!("{query} site:{s}"));
    let max = p.count.unwrap_or(DEFAULT_RESULTS).clamp(1, MAX_RESULTS);
    let cancel = ctx.cancel.clone();
    let html: String = tokio::runtime::Handle::current().block_on(async {
        let send = async {
            let fail = |e: reqwest::Error| ToolError::new(if e.is_timeout() { ToolErrorKind::Timeout } else { ToolErrorKind::Failed }, format!("search failed: {e}"));
            let url = url::Url::parse_with_params(SEARCH_URL, [("q", q.as_str()), ("kl", "wt-wt")]).map_err(|e| ToolError::invalid(e.to_string()))?;
            let r = client()?.get(url).send().await.map_err(fail)?;
            let status = r.status();
            let body = r.text().await.map_err(fail)?;
            if !status.is_success() {
                return Err(ToolError::new(ToolErrorKind::Failed, format!("the search service answered HTTP {status}; try again, or fetch a known docs URL with web_fetch")));
            }
            Ok(body)
        };
        tokio::select! {
            _ = cancel.cancelled() => Err(ToolError::new(ToolErrorKind::Cancelled, "cancelled")),
            r = send => r,
        }
    })?;
    let results = parse_results(&html, max);
    Ok(json!({
        "query": q,
        "results": results,
        "note": if results.is_empty() { "No results. Try other words, or fewer of them." } else { "Read a page with web_fetch for its full text." },
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FetchInput {
    url: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

fn web_fetch(ctx: &ToolCtx, input: serde_json::Value) -> ToolResult {
    let p: FetchInput = parse(input)?;
    let url = url::Url::parse(p.url.trim()).map_err(|e| ToolError::invalid(format!("invalid url: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ToolError::invalid("only http:// and https:// URLs with a host can be fetched"));
    }
    let source = match p.format.as_deref() {
        None | Some("text") => false,
        Some("source") => true,
        Some(f) => return Err(ToolError::invalid(format!("format must be text or source, not {f}"))),
    };
    check_target(&url)?;
    let cancel = ctx.cancel.clone();
    let (status, final_url, content_type, body, cut) = tokio::runtime::Handle::current().block_on(async {
        tokio::select! {
            _ = cancel.cancelled() => Err(ToolError::new(ToolErrorKind::Cancelled, "cancelled")),
            r = fetch(url) => r,
        }
    })?;
    let html = content_type.contains("html") || (content_type.is_empty() && body.trim_start().starts_with('<'));
    let text = if html && !source { html_to_text(&body) } else { body };
    let offset = p.offset.unwrap_or(0);
    let max = p.max_chars.unwrap_or(DEFAULT_CHARS).clamp(1, MAX_CHARS);
    let total = text.chars().count();
    let part: String = text.chars().skip(offset).take(max).collect();
    let more = offset + part.chars().count() < total;
    if !(200..400).contains(&status) && part.trim().is_empty() {
        return Err(ToolError::new(ToolErrorKind::Failed, format!("{final_url} answered HTTP {status}")));
    }
    Ok(json!({
        "url": final_url,
        "status": status,
        "contentType": content_type,
        "content": part,
        "totalChars": total,
        "nextOffset": more.then_some(offset + max),
        "bodyTruncated": cut,
    }))
}

/// GET with a size cap. Returns status, final URL, content type, body text, and whether the body was cut.
async fn fetch(url: url::Url) -> Result<(u16, String, String, String, bool), ToolError> {
    let fail = |e: reqwest::Error| {
        ToolError::new(if e.is_timeout() { ToolErrorKind::Timeout } else { ToolErrorKind::Failed }, format!("fetch failed: {e}"))
    };
    let mut res = client()?.get(url).send().await.map_err(fail)?;
    let status = res.status().as_u16();
    let final_url = res.url().to_string();
    let content_type =
        res.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    if ["image/", "audio/", "video/", "font/", "application/octet-stream", "application/zip", "application/pdf"]
        .iter()
        .any(|t| content_type.starts_with(t))
    {
        return Err(ToolError::invalid(format!("{final_url} is {content_type}, not text")));
    }
    let mut buf = Vec::new();
    let mut cut = false;
    while let Some(chunk) = res.chunk().await.map_err(fail)? {
        let room = MAX_BYTES - buf.len();
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if chunk.len() > room {
            cut = true;
            break;
        }
    }
    Ok((status, final_url, content_type, String::from_utf8_lossy(&buf).into_owned(), cut))
}

static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("regex"));
static HREF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)<a\b[^>]*\bhref\s*=\s*["']([^"'#]+)"#).expect("regex"));

/// One page for docs indexing: its final URL, title, readable text, and the http(s) links on it.
/// Local and private addresses are refused, as for `web_fetch`.
pub async fn fetch_page(url: &url::Url) -> Result<(url::Url, String, String, Vec<url::Url>), ToolError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::invalid("only http:// and https:// URLs can be indexed"));
    }
    check_target(url)?;
    let (status, final_url, content_type, body, _) = fetch(url.clone()).await?;
    if !(200..300).contains(&status) {
        return Err(ToolError::new(ToolErrorKind::Failed, format!("{final_url} answered HTTP {status}")));
    }
    let final_url = url::Url::parse(&final_url).unwrap_or_else(|_| url.clone());
    let html = content_type.contains("html") || body.trim_start().starts_with('<');
    if !html {
        return Ok((final_url, String::new(), body, Vec::new()));
    }
    let title = TITLE.captures(&body).map(|c| html_to_text(&c[1])).unwrap_or_default();
    let links = HREF
        .captures_iter(&body)
        .filter_map(|c| final_url.join(c[1].trim()).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .map(|mut u| {
            u.set_fragment(None);
            u
        })
        .collect();
    Ok((final_url, title, html_to_text(&body), links))
}

static DROP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(script|style|noscript|svg|template)\b.*?</(script|style|noscript|svg|template)\s*>|<!--.*?-->").expect("regex")
});
static BREAK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(br|/p|/div|/li|/tr|/h[1-6]|/pre|/section|/article|/header|/footer|/table)\b[^>]*>").expect("regex")
});
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").expect("regex"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\r\f\v]+").expect("regex"));
static BLANKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n+").expect("regex"));

/// HTML reduced to its visible text: scripts, styles, and tags removed, common entities decoded.
fn html_to_text(html: &str) -> String {
    let s = DROP.replace_all(html, " ");
    let s = BREAK.replace_all(&s, "\n");
    let s = TAG.replace_all(&s, " ");
    let s = [("&nbsp;", " "), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&#39;", "'"), ("&apos;", "'"), ("&amp;", "&")]
        .iter()
        .fold(s.into_owned(), |acc, (from, to)| acc.replace(from, to));
    let s = SPACES.replace_all(&s, " ");
    let s: String = s.lines().map(str::trim).collect::<Vec<_>>().join("\n");
    BLANKS.replace_all(&s, "\n\n").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tests::{ctx, run};

    #[test]
    fn html_becomes_readable_text() {
        let html = "<html><head><style>p{color:red}</style><script>alert(1)</script></head><body><h1>Widgets</h1><p>Use &lt;div id=&quot;w&quot;&gt; &amp; a script.</p><!-- hidden --><ul><li>One</li><li>Two</li></ul></body></html>";
        assert_eq!(html_to_text(html), "Widgets\nUse <div id=\"w\"> & a script.\nOne\nTwo");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_http_urls_are_fetched() {
        let c = ctx(&[("a.txt", "x\n")]);
        for bad in ["file:///etc/passwd", "ftp://example.com/a", "not a url", "https://"] {
            let c = c.clone();
            let bad = bad.to_owned();
            let r = tokio::task::spawn_blocking(move || run(&c, "web_fetch", json!({ "url": bad }))).await.unwrap();
            assert_eq!(r.unwrap_err().kind, ToolErrorKind::InvalidInput);
        }
        let c2 = c.clone();
        let r = tokio::task::spawn_blocking(move || run(&c2, "web_fetch", json!({ "url": "https://example.com", "format": "pdf" })))
            .await
            .unwrap();
        assert_eq!(r.unwrap_err().kind, ToolErrorKind::InvalidInput);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_tool_refuses_local_and_private_targets() {
        // A page the agent read can point it at the machine it runs on; the call is refused before
        // any request is made, so a metadata endpoint is never reached.
        let c = ctx(&[("a.txt", "x\n")]);
        for bad in ["http://127.0.0.1:11434/api/tags", "http://169.254.169.254/latest/meta-data/", "http://localhost:8080/"] {
            let c = c.clone();
            let bad = bad.to_owned();
            let r = tokio::task::spawn_blocking(move || run(&c, "web_fetch", json!({ "url": bad }))).await.unwrap();
            assert_eq!(r.unwrap_err().kind, ToolErrorKind::Forbidden);
        }
    }

    #[test]
    fn search_results_are_read_from_the_results_page() {
        let html = r#"<div class="result"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Fserde&amp;rut=x">serde - <b>Rust</b></a>
            <a class="result__snippet" href="x">A <b>serialization</b> framework.</a></div>
            <div class="result"><a class="result__a" href="https://duckduckgo.com/y.js?ad=1">Ad</a><a class="result__snippet">ad</a></div>
            <div class="result"><a class="result__a" href="https://serde.rs/">Serde</a><a class="result__snippet">Home</a></div>"#;
        let r = parse_results(html, 5);
        assert_eq!(r.len(), 2, "the ad is left out: {r:?}");
        assert_eq!((r[0]["url"].as_str(), r[0]["title"].as_str(), r[0]["snippet"].as_str()), (Some("https://docs.rs/serde"), Some("serde - Rust"), Some("A serialization framework.")));
        assert_eq!(r[1]["url"], "https://serde.rs/");
        assert_eq!(parse_results(html, 1).len(), 1);
    }

    #[test]
    fn private_and_local_addresses_are_refused() {
        let ok = |u: &str| check_target(&url::Url::parse(u).unwrap()).is_ok();
        // The machine itself, its network, and cloud metadata endpoints.
        for u in [
            "http://127.0.0.1:11434/api/tags",
            "http://127.1.2.3/",
            "http://localhost:8080/",
            "http://LOCALHOST/admin",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:3000/",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://0.0.0.0:8080/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://host.docker.internal:2375/",
            "http://printer.local/",
        ] {
            assert!(!ok(u), "{u} must not be fetchable");
        }
        // A public address stays fetchable; DNS lookups are not needed for literal addresses.
        for u in ["https://1.1.1.1/", "https://[2606:4700:4700::1111]/"] {
            assert!(ok(u), "{u} is a public address");
        }
    }
}
