//! Provider abstraction: adapters turn internal requests into wire requests and wire streams into
//! normalized `StreamEvent`s. Everything provider-specific stays in the adapter modules.

pub mod anthropic;
pub mod ollama;
pub mod openai;
pub mod stream;
pub mod types;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use stream::{LineBuffer, StreamDecoder};
use tokio_util::sync::CancellationToken;
pub use types::*;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ERROR_BODY: usize = 64 * 1024;
const MAX_ERROR_MESSAGE: usize = 500;
const MAX_LISTED_MODELS: usize = 500;
const SENSITIVE_HEADERS: [&str; 2] = ["authorization", "x-api-key"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<Value>,
}

impl HttpRequest {
    pub fn get(url: String, headers: Vec<(&'static str, String)>) -> Self {
        Self { method: Method::Get, url, headers, body: None }
    }

    pub fn post(url: String, headers: Vec<(&'static str, String)>, body: Value) -> Self {
        Self { method: Method::Post, url, headers, body: Some(body) }
    }
}

fn chat_request(cfg: &ResolvedConfig, req: &ChatRequest) -> HttpRequest {
    match cfg.kind {
        ProviderKind::Anthropic => anthropic::chat_request(cfg, req),
        ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => openai::chat_request(cfg, req),
        ProviderKind::Ollama => ollama::chat_request(cfg, req),
    }
}

fn decoder(kind: ProviderKind) -> Box<dyn StreamDecoder> {
    match kind {
        ProviderKind::Anthropic => Box::<anthropic::Decoder>::default(),
        ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => Box::<openai::Decoder>::default(),
        ProviderKind::Ollama => Box::<ollama::Decoder>::default(),
    }
}

/// Pulls a human-readable message out of the common provider error body shapes.
pub fn error_message(body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"].as_str().or(v["error"].as_str()).or(v["message"].as_str()).or(v["detail"].as_str()).map(str::to_owned)
        })
        .unwrap_or_else(|| body.trim().to_owned());
    let msg = if msg.is_empty() { "request failed".to_owned() } else { msg };
    match msg.char_indices().nth(MAX_ERROR_MESSAGE) {
        Some((i, _)) => format!("{}…", &msg[..i]),
        None => msg,
    }
}

fn map_reqwest(e: reqwest::Error) -> ProviderError {
    let e = e.without_url();
    if e.is_timeout() {
        ProviderError::new(ErrorKind::Timeout, "request timed out")
    } else if e.is_connect() {
        ProviderError::new(ErrorKind::Network, format!("could not connect: {e}"))
    } else {
        ProviderError::new(ErrorKind::Network, e.to_string())
    }
}

fn cancelled() -> ProviderError {
    ProviderError::new(ErrorKind::Cancelled, "request cancelled")
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestReport {
    pub latency_ms: u64,
    pub models: Vec<String>,
    /// `None` when the endpoint cannot enumerate models.
    pub model_available: Option<bool>,
    pub capabilities: Capabilities,
}

#[derive(Clone)]
pub struct Providers {
    http: reqwest::Client,
    model_info_cache: std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, (usize, std::time::Instant)>>>,
}

impl Providers {
    pub fn new() -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("task-x-runtime/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ProviderError::new(ErrorKind::InvalidConfig, format!("failed to build HTTP client: {e}")))?;
        Ok(Self { http, model_info_cache: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())) })
    }

    async fn send(&self, req: HttpRequest, timeout: Duration, cancel: &CancellationToken) -> Result<reqwest::Response, ProviderError> {
        let mut headers = HeaderMap::new();
        for (k, v) in req.headers {
            let mut hv = HeaderValue::from_str(&v)
                .map_err(|_| ProviderError::new(ErrorKind::InvalidConfig, format!("invalid value for header {k}")))?;
            hv.set_sensitive(SENSITIVE_HEADERS.contains(&k));
            headers.insert(HeaderName::from_static(k), hv);
        }
        let rb = match req.method {
            Method::Get => self.http.get(&req.url),
            Method::Post => self.http.post(&req.url).json(req.body.as_ref().unwrap_or(&Value::Null)),
        }
        .headers(headers);
        let res = tokio::select! {
            _ = cancel.cancelled() => return Err(cancelled()),
            r = tokio::time::timeout(timeout, rb.send()) => r.map_err(|_| ProviderError::new(ErrorKind::Timeout, "timed out waiting for response headers"))?.map_err(map_reqwest)?,
        };
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let body = tokio::select! {
            _ = cancel.cancelled() => return Err(cancelled()),
            b = tokio::time::timeout(timeout, read_bounded(res, MAX_ERROR_BODY)) => b.unwrap_or_default(),
        };
        Err(ProviderError::from_status(status.as_u16(), error_message(&body)))
    }

    async fn get_json(&self, req: HttpRequest, timeout: Duration, cancel: &CancellationToken) -> Result<Value, ProviderError> {
        let res = self.send(req, timeout, cancel).await?;
        tokio::select! {
            _ = cancel.cancelled() => Err(cancelled()),
            r = tokio::time::timeout(timeout, res.json::<Value>()) => r
                .map_err(|_| ProviderError::new(ErrorKind::Timeout, "timed out reading response"))?
                .map_err(|e| ProviderError::protocol(format!("invalid JSON response: {}", e.without_url()))),
        }
    }

    /// Streams a chat completion. `on_event` sees every normalized event in order; the returned
    /// outcome aggregates them. `cfg.timeout` bounds both the wait for headers and each idle gap.
    pub async fn stream_chat(
        &self,
        cfg: &ProviderConfig,
        req: &ChatRequest,
        cancel: &CancellationToken,
        mut on_event: impl FnMut(&StreamEvent),
    ) -> Result<ChatOutcome, ProviderError> {
        let cfg = cfg.resolve()?;
        req.validate()?;
        let res = self.send(chat_request(&cfg, req), cfg.timeout, cancel).await?;
        let mut body = res.bytes_stream();
        let mut lines = LineBuffer::default();
        let mut dec = decoder(cfg.kind);
        let mut out = ChatOutcome::default();
        let mut emit = |evs: Vec<StreamEvent>, out: &mut ChatOutcome| {
            for e in &evs {
                out.apply(e);
                on_event(e);
            }
        };
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(cancelled()),
                c = tokio::time::timeout(cfg.timeout, body.next()) => c.map_err(|_| ProviderError::new(ErrorKind::Timeout, "stream stalled"))?,
            };
            let Some(chunk) = chunk else { break };
            for line in lines.push(&chunk.map_err(map_reqwest)?)? {
                emit(dec.feed_line(&line)?, &mut out);
            }
        }
        if let Some(rest) = lines.finish()? {
            emit(dec.feed_line(&rest)?, &mut out);
        }
        emit(dec.finish()?, &mut out);
        Ok(out)
    }

    pub async fn list_models(&self, cfg: &ProviderConfig, cancel: &CancellationToken) -> Result<Vec<String>, ProviderError> {
        let cfg = cfg.resolve()?;
        let (req, parse): (_, fn(&Value) -> Vec<String>) = match cfg.kind {
            ProviderKind::Anthropic => (anthropic::models_request(&cfg), anthropic::parse_models),
            ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => (openai::models_request(&cfg), openai::parse_models),
            ProviderKind::Ollama => (ollama::models_request(&cfg), ollama::parse_models),
        };
        let mut models = parse(&self.get_json(req, cfg.timeout, cancel).await?);
        models.sort();
        models.dedup();
        models.truncate(MAX_LISTED_MODELS);
        Ok(models)
    }

    /// Verifies connectivity and credentials, and refines capabilities where the endpoint exposes them.
    pub async fn test(&self, cfg: &ProviderConfig, cancel: &CancellationToken) -> Result<TestReport, ProviderError> {
        let resolved = cfg.resolve()?;
        let started = Instant::now();
        let mut caps = resolved.kind.capabilities();
        let (models, model_available) = match self.list_models(cfg, cancel).await {
            Ok(m) => {
                let available = m.iter().any(|n| *n == resolved.model || n.strip_suffix(":latest") == Some(resolved.model.as_str()));
                (m, Some(available))
            }
            // Many OpenAI-compatible servers do not implement /models; fall back to a one-token chat.
            Err(e) if resolved.kind == ProviderKind::OpenAiCompatible && matches!(e.kind, ErrorKind::NotFound) => {
                let ping = ChatRequest {
                    messages: vec![Message { role: Role::User, content: vec![ContentPart::Text { text: "ping".into() }] }],
                    max_tokens: Some(1),
                    ..Default::default()
                };
                self.stream_chat(cfg, &ping, cancel, |_| {}).await?;
                (Vec::new(), None)
            }
            Err(e) => return Err(e),
        };
        if resolved.kind == ProviderKind::Ollama {
            if let Ok(show) = self.get_json(ollama::show_request(&resolved), resolved.timeout, cancel).await {
                caps = ollama::refine_capabilities(caps, &show);
            }
        }
        Ok(TestReport { latency_ms: started.elapsed().as_millis() as u64, models, model_available, capabilities: caps })
    }
}

async fn read_bounded(res: reqwest::Response, limit: usize) -> String {
    let mut s = res.bytes_stream();
    let mut buf = Vec::new();
    while let Some(Ok(c)) = s.next().await {
        buf.extend_from_slice(&c[..c.len().min(limit - buf.len())]);
        if buf.len() >= limit {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// The window the provider reports for `p.model` (cached for 5 minutes), or the built-in estimate
/// when the API does not say. Errors only for an invalid provider config.
pub async fn context_window_dynamic(providers: &Providers, p: &ProviderConfig, cancel: &CancellationToken) -> Result<usize, ProviderError> {
    const TTL: std::time::Duration = std::time::Duration::from_secs(300);
    let cache_key = format!("{:?}:{}:{:?}", p.kind, p.model, p.base_url);
    if let Some((window, at)) = providers.model_info_cache.read().await.get(&cache_key) {
        if at.elapsed() < TTL {
            return Ok(*window);
        }
    }
    let cfg = p.resolve()?;
    // A failed lookup is cached too, so an endpoint without metadata is asked once per TTL, not every turn.
    let reported = match p.kind {
        ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => {
            let res = providers.get_json(openai::models_request(&cfg), cfg.timeout, cancel).await;
            res.ok().and_then(|v| openai::parse_model_info(&p.model, &v))
        }
        ProviderKind::Anthropic => {
            let res = providers.get_json(anthropic::models_request(&cfg), cfg.timeout, cancel).await;
            res.ok().and_then(|v| anthropic::parse_context_window(&p.model, &v))
        }
        ProviderKind::Ollama => {
            let res = providers.get_json(ollama::show_request(&cfg), cfg.timeout, cancel).await;
            res.ok().and_then(|v| ollama::parse_context_window(&v))
        }
    };
    let window = reported.unwrap_or_else(|| context_window(p));
    providers.model_info_cache.write().await.insert(cache_key, (window, std::time::Instant::now()));
    Ok(window)
}

/// Legacy context window function (static fallback)
pub fn context_window(p: &ProviderConfig) -> usize {
    match p.kind {
        ProviderKind::Anthropic => anthropic::context_window(&p.model),
        ProviderKind::OpenAi => openai::context_window(&p.model),
        ProviderKind::Ollama => ollama::DEFAULT_CONTEXT_WINDOW,
        // Unknown servers: a common size that most hosted and local models support.
        ProviderKind::OpenAiCompatible => 32_768,
    }
}

pub fn capability_table() -> Value {
    ProviderKind::ALL
        .iter()
        .map(|k| json!({ "kind": k, "capabilities": k.capabilities(), "requiresApiKey": k.requires_api_key() }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: ProviderKind, base: Option<&str>, key: Option<&str>) -> ProviderConfig {
        ProviderConfig { kind, model: "m".into(), base_url: base.map(Into::into), api_key: key.map(Into::into), timeout_ms: Some(5) }
    }

    #[test]
    fn config_validation_rules() {
        assert_eq!(cfg(ProviderKind::Anthropic, None, None).resolve().err().unwrap().kind, ErrorKind::Auth);
        assert_eq!(cfg(ProviderKind::OpenAiCompatible, None, None).resolve().err().unwrap().kind, ErrorKind::InvalidConfig);
        assert_eq!(cfg(ProviderKind::OpenAiCompatible, Some("https://x/v1"), None).resolve().err().unwrap().kind, ErrorKind::Auth);
        assert!(cfg(ProviderKind::Ollama, None, None).resolve().is_ok());
        assert!(cfg(ProviderKind::OpenAiCompatible, Some("http://evil.example/v1"), Some("k")).resolve().is_err());
        assert!(cfg(ProviderKind::OpenAiCompatible, Some("file:///etc"), None).resolve().is_err());
        assert!(cfg(ProviderKind::OpenAiCompatible, Some("https://u:p@host/v1"), None).resolve().is_err());
        let ok = cfg(ProviderKind::OpenAiCompatible, Some("http://[::1]:8080/v1"), Some("k")).resolve().unwrap();
        assert_eq!(ok.timeout, Duration::from_millis(MIN_TIMEOUT_MS));
        assert_eq!(ok.endpoint("/models"), "http://[::1]:8080/v1/models");
    }

    #[test]
    fn debug_output_redacts_api_key() {
        let s = format!("{:?}", cfg(ProviderKind::OpenAi, None, Some("sk-secret")));
        assert!(!s.contains("sk-secret") && s.contains("<redacted>"));
    }

    #[test]
    fn extracts_error_messages_from_common_shapes() {
        assert_eq!(error_message(r#"{"error":{"message":"bad key","type":"x"}}"#), "bad key");
        assert_eq!(error_message(r#"{"error":"model missing"}"#), "model missing");
        assert_eq!(error_message("upstream exploded"), "upstream exploded");
        assert_eq!(error_message(""), "request failed");
        assert!(error_message(&"x".repeat(2000)).chars().count() <= MAX_ERROR_MESSAGE + 1);
    }

    #[test]
    fn status_codes_map_to_kinds_and_retryability() {
        let e = ProviderError::from_status(429, "slow down");
        assert_eq!((e.kind, e.retryable, e.status), (ErrorKind::RateLimited, true, Some(429)));
        assert_eq!(ProviderError::from_status(401, "").kind, ErrorKind::Auth);
        assert!(!ProviderError::from_status(400, "").retryable);
        assert_eq!(ProviderError::from_status(529, "").kind, ErrorKind::Overloaded);
        assert_eq!(ProviderError::from_status(502, "").kind, ErrorKind::Server);
    }

    #[test]
    fn reads_the_context_window_each_provider_reports() {
        let ollama = json!({ "model_info": { "general.architecture": "qwen3", "qwen3.context_length": 40960 } });
        assert_eq!(ollama::parse_context_window(&ollama), Some(40_960));
        assert_eq!(ollama::parse_context_window(&json!({ "model_info": {} })), None);
        let anthropic = json!({ "data": [{ "id": "claude-x", "max_input_tokens": 300_000 }] });
        assert_eq!(anthropic::parse_context_window("claude-x", &anthropic), Some(300_000));
        assert_eq!(anthropic::parse_context_window("other", &anthropic), None);
        let openai = json!({ "data": [{ "id": "m", "context_length": 64_000 }] });
        assert_eq!(openai::parse_model_info("m", &openai), Some(64_000));
    }

    #[test]
    fn stop_reasons_normalize_across_providers() {
        assert_eq!(StopReason::from_provider("stop"), StopReason::EndTurn);
        assert_eq!(StopReason::from_provider("length"), StopReason::MaxTokens);
        assert_eq!(StopReason::from_provider("tool_calls"), StopReason::ToolUse);
        assert_eq!(StopReason::from_provider("weird"), StopReason::Unknown);
    }
}
