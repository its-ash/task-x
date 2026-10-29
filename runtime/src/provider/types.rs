use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::time::Duration;

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MIN_TIMEOUT_MS: u64 = 1_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const DEFAULT_MAX_TOKENS: u32 = 4_096;
const MAX_MODEL_LEN: usize = 256;
/// Images one prompt may carry, and the base64 size of each (about 5 MB of image data).
pub const MAX_IMAGES: usize = 8;
const MAX_IMAGE_B64: usize = 7_000_000;
const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openaiCompatible")]
    OpenAiCompatible,
    Ollama,
}

impl ProviderKind {
    pub const ALL: [Self; 4] = [Self::Anthropic, Self::OpenAi, Self::OpenAiCompatible, Self::Ollama];

    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::OpenAi => Some("https://api.openai.com/v1"),
            Self::Ollama => Some("http://127.0.0.1:11434"),
            Self::OpenAiCompatible => None,
        }
    }

    pub fn requires_api_key(self) -> bool {
        !matches!(self, Self::Ollama)
    }

    /// Static capabilities; `provider/test` may refine them by probing the endpoint.
    pub fn capabilities(self) -> Capabilities {
        Capabilities {
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: !matches!(self, Self::Ollama),
            system_prompt: true,
            message_format: match self {
                Self::Anthropic => MessageFormat::AnthropicMessages,
                Self::OpenAi | Self::OpenAiCompatible => MessageFormat::OpenAiChat,
                Self::Ollama => MessageFormat::OllamaChat,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageFormat {
    AnthropicMessages,
    OpenAiChat,
    OllamaChat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub parallel_tool_calls: bool,
    pub system_prompt: bool,
    pub message_format: MessageFormat,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("kind", &self.kind)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

/// A config that passed validation: base URL resolved, timeout clamped, key present when required.
#[derive(Clone)]
pub struct ResolvedConfig {
    pub kind: ProviderKind,
    pub model: String,
    pub base_url: url::Url,
    pub api_key: Option<String>,
    pub timeout: Duration,
}

impl ProviderConfig {
    pub fn resolve(&self) -> Result<ResolvedConfig, ProviderError> {
        let invalid = |m: &str| ProviderError::new(ErrorKind::InvalidConfig, m);
        let model = self.model.trim();
        if model.is_empty() || model.len() > MAX_MODEL_LEN || model.chars().any(char::is_control) {
            return Err(invalid("model must be a non-empty name without control characters"));
        }
        let raw = self
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or(self.kind.default_base_url())
            .ok_or_else(|| invalid("baseUrl is required for OpenAI-compatible providers"))?;
        let base_url = url::Url::parse(raw.trim_end_matches('/')).map_err(|e| invalid(&format!("invalid baseUrl: {e}")))?;
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(invalid("baseUrl must use http or https"));
        }
        if !base_url.username().is_empty() || base_url.password().is_some() {
            return Err(invalid("baseUrl must not embed credentials"));
        }
        let api_key = self.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned);
        if let Some(k) = &api_key {
            if k.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(invalid("apiKey contains invalid characters"));
            }
            if base_url.scheme() == "http" && !is_loopback(&base_url) {
                return Err(invalid("refusing to send an API key over plain http to a non-loopback host"));
            }
        } else if self.kind.requires_api_key() {
            return Err(ProviderError::new(ErrorKind::Auth, "an API key is required for this provider"));
        }
        let ms = self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        Ok(ResolvedConfig { kind: self.kind, model: model.to_owned(), base_url, api_key, timeout: Duration::from_millis(ms) })
    }
}

fn is_loopback(u: &url::Url) -> bool {
    match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

impl ResolvedConfig {
    /// Joins `path` onto the base URL, preserving any base path prefix (e.g. `/v1`).
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.as_str().trim_end_matches('/'), path.trim_start_matches('/'))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentPart {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        tool_name: Option<String>,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// A base64 image the user attached (`image/png`, `image/jpeg`, `image/gif`, or `image/webp`).
    #[serde(rename_all = "camelCase")]
    Image {
        media_type: String,
        data: String,
    },
}

/// One model an endpoint offers, with what it reports (or what is known) about it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    /// Input window in tokens; absent when neither the endpoint nor the built-in table knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Any of `tools`, `vision`, `thinking`, `embedding`, in that order; empty when unknown.
    pub capabilities: Vec<String>,
    /// Size and quantization of a local model, such as `8.0B · Q4_K_M`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl ModelInfo {
    pub fn new(id: &str, context_window: Option<usize>, capabilities: &[&str]) -> Self {
        let order = ["tools", "vision", "thinking", "embedding"];
        let capabilities = order.iter().filter(|c| capabilities.contains(c)).map(|c| (*c).to_owned()).collect();
        Self { id: id.to_owned(), context_window, capabilities, details: None }
    }
}

/// An image attached to a prompt: `{ mediaType, data }`, where `data` is base64 without a `data:` prefix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageInput {
    pub media_type: String,
    pub data: String,
}

/// Checks attached images and turns them into content parts.
pub fn image_parts(images: &[ImageInput]) -> Result<Vec<ContentPart>, String> {
    if images.len() > MAX_IMAGES {
        return Err(format!("at most {MAX_IMAGES} images per prompt"));
    }
    images
        .iter()
        .enumerate()
        .map(|(i, im)| {
            if !IMAGE_TYPES.contains(&im.media_type.as_str()) {
                return Err(format!("images[{i}]: mediaType must be one of {IMAGE_TYPES:?}"));
            }
            let ok = !im.data.is_empty()
                && im.data.len() <= MAX_IMAGE_B64
                && im.data.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b));
            if !ok {
                return Err(format!("images[{i}]: data must be base64, 1..={MAX_IMAGE_B64} characters"));
            }
            Ok(ContentPart::Image { media_type: im.media_type.clone(), data: im.data.clone() })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl Message {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatRequest {
    #[serde(default)]
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Context window to ask the server for, where the API takes one (Ollama `num_ctx`).
    #[serde(default)]
    pub context_window: Option<u32>,
    /// How hard the model thinks, for models that take a level. Unset: the model's default.
    #[serde(default)]
    pub effort: Option<Effort>,
}

/// Reasoning effort, lowest to highest. Each provider maps it to what the model takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The three-level scale most APIs use (`low`, `medium`, `high`): higher levels become `high`.
    pub fn three_level(self) -> &'static str {
        self.min(Self::High).as_str()
    }
}

impl ChatRequest {
    pub fn validate(&self) -> Result<(), ProviderError> {
        let invalid = |m: &str| Err(ProviderError::new(ErrorKind::InvalidRequest, m));
        if self.messages.is_empty() {
            return invalid("messages must not be empty");
        }
        if self.messages.first().is_some_and(|m| m.role != Role::User) {
            return invalid("the first message must have role 'user'");
        }
        if self.max_tokens == Some(0) {
            return invalid("maxTokens must be greater than zero");
        }
        if self.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
            return invalid("temperature must be between 0 and 2");
        }
        if let Some(t) = self.tools.iter().find(|t| t.name.is_empty() || !t.input_schema.is_object()) {
            return invalid(&format!("tool '{}' needs a name and an object inputSchema", t.name));
        }
        Ok(())
    }

    pub fn max_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)
    }
}

const LOST_RESULT: &str =
    "No result was recorded for this call: the run stopped before it was saved. Do not assume it succeeded; check the current state again.";

fn lost_result(tool_use_id: &str, tool_name: Option<&str>) -> ContentPart {
    ContentPart::ToolResult {
        tool_use_id: tool_use_id.to_owned(),
        tool_name: tool_name.map(str::to_owned),
        content: LOST_RESULT.to_owned(),
        is_error: true,
    }
}

/// The calls an assistant turn made, in order.
fn calls_of(m: &Message) -> Vec<(String, Option<String>)> {
    if m.role != Role::Assistant {
        return Vec::new();
    }
    m.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolUse { id, name, .. } => Some((id.clone(), Some(name.clone()))),
            _ => None,
        })
        .collect()
}

/// Restores the tool-call contract every provider enforces: each tool call is answered in the
/// message that follows it, and a result never stands without the call it belongs to. A run that
/// stopped between a call and its result leaves a history the API rejects as a whole
/// ("insufficient tool messages following tool_calls message"), so histories are repaired before
/// they are sent. Returns how many parts were added or rewritten.
pub fn repair_history(messages: &mut Vec<Message>) -> usize {
    let mut fixed = 0;
    // 1. Every call gets a turn right after it, even when nothing answered it.
    let mut i = 0;
    while i < messages.len() {
        let calls = calls_of(&messages[i]);
        if calls.is_empty() {
            i += 1;
            continue;
        }
        if !messages.get(i + 1).is_some_and(|m| m.role == Role::User) {
            let content = calls.iter().map(|(id, name)| lost_result(id, name.as_deref())).collect();
            messages.insert(i + 1, Message { role: Role::User, content });
            fixed += calls.len();
        }
        i += 2;
    }
    // 2. That turn answers every call of the one before, and nothing else. Results come first, so
    //    a prompt or hook note in the same turn stays behind them.
    let mut i = 0;
    while i + 1 < messages.len() {
        let calls = calls_of(&messages[i]);
        let next = &mut messages[i + 1].content;
        for p in next.iter_mut() {
            let ContentPart::ToolResult { tool_use_id, content, .. } = p else { continue };
            if !calls.iter().any(|(id, _)| id == tool_use_id) {
                *p = ContentPart::Text { text: format!("{tool_use_id} returned: {content}") };
                fixed += 1;
            }
        }
        let mut at = next.iter().take_while(|p| matches!(p, ContentPart::ToolResult { .. })).count();
        let answered: Vec<String> = next
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        for (id, name) in &calls {
            if !answered.contains(id) {
                next.insert(at, lost_result(id, name.as_deref()));
                at += 1;
                fixed += 1;
            }
        }
        i += 1;
    }
    fixed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    #[default]
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    ContentFilter,
    Unknown,
}

impl StopReason {
    /// Maps every provider's native stop/finish reason onto the internal enum.
    pub fn from_provider(s: &str) -> Self {
        match s {
            "end_turn" | "stop" => Self::EndTurn,
            "max_tokens" | "length" => Self::MaxTokens,
            "tool_use" | "tool_calls" | "function_call" => Self::ToolUse,
            "stop_sequence" => Self::StopSequence,
            "content_filter" | "refusal" => Self::ContentFilter,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// All prompt tokens, cached ones included.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Prompt tokens the provider read from its prompt cache (billed at a lower rate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StreamEvent {
    TextDelta { text: String },
    ToolCall { id: String, name: String, input: Value },
    Usage { usage: Usage },
    Done { stop_reason: StopReason },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatOutcome {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

impl ChatOutcome {
    pub fn apply(&mut self, ev: &StreamEvent) {
        match ev {
            StreamEvent::TextDelta { text } => self.text.push_str(text),
            StreamEvent::ToolCall { id, name, input } => {
                self.tool_calls.push(ToolCall { id: id.clone(), name: name.clone(), input: input.clone() })
            }
            StreamEvent::Usage { usage } => {
                self.usage.input_tokens = usage.input_tokens.or(self.usage.input_tokens);
                self.usage.output_tokens = usage.output_tokens.or(self.usage.output_tokens);
                self.usage.cached_tokens = usage.cached_tokens.or(self.usage.cached_tokens);
            }
            StreamEvent::Done { stop_reason } => self.stop_reason = *stop_reason,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    InvalidConfig,
    InvalidRequest,
    Auth,
    PermissionDenied,
    NotFound,
    RateLimited,
    Overloaded,
    Server,
    Network,
    Timeout,
    Cancelled,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{message}")]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub retryable: bool,
}

impl ProviderError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        let retryable =
            matches!(kind, ErrorKind::RateLimited | ErrorKind::Overloaded | ErrorKind::Server | ErrorKind::Network | ErrorKind::Timeout);
        Self { kind, message: message.into(), status: None, retryable }
    }

    /// Maps an HTTP status and (already-extracted) provider error message into a normalized error.
    pub fn from_status(status: u16, message: impl Into<String>) -> Self {
        let kind = match status {
            400 | 413 | 422 => ErrorKind::InvalidRequest,
            401 => ErrorKind::Auth,
            403 => ErrorKind::PermissionDenied,
            404 => ErrorKind::NotFound,
            408 => ErrorKind::Timeout,
            429 => ErrorKind::RateLimited,
            529 | 503 => ErrorKind::Overloaded,
            _ if status >= 500 => ErrorKind::Server,
            _ => ErrorKind::Protocol,
        };
        Self { status: Some(status), ..Self::new(kind, message) }
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Protocol, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message { role: Role::User, content: vec![ContentPart::Text { text: text.into() }] }
    }

    fn call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse { id: id.into(), name: "read_file".into(), input: json!({ "path": "a.txt" }) }],
        }
    }

    fn result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                tool_name: Some("read_file".into()),
                content: "x".into(),
                is_error: false,
            }],
        }
    }

    fn ids(m: &Message) -> Vec<&str> {
        m.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_valid_history_is_left_alone() {
        let mut m = vec![user("q"), call("c1"), result("c1"), user("next")];
        let before = m.clone();
        assert_eq!(repair_history(&mut m), 0);
        assert_eq!(m, before);
    }

    #[test]
    fn a_call_whose_result_was_lost_gets_one() {
        // The shape a cancelled run leaves behind: the assistant asked, nothing answered.
        let mut m = vec![user("q"), call("c1")];
        assert_eq!(repair_history(&mut m), 1);
        assert_eq!(m.len(), 3);
        assert_eq!(m[2].role, Role::User);
        assert_eq!(ids(&m[2]), ["c1"]);
        assert!(matches!(&m[2].content[0], ContentPart::ToolResult { is_error: true, .. }), "an unknown result is not a success");
    }

    #[test]
    fn the_follow_up_prompt_does_not_swallow_the_call() {
        // A follow-up turn that replaced the results: the prompt keeps its text, the call its answer.
        let mut m = vec![user("q"), call("c1"), user("do it differently")];
        assert_eq!(repair_history(&mut m), 1);
        assert_eq!(m.len(), 3);
        assert_eq!(ids(&m[2]), ["c1"]);
        assert_eq!(m[2].content.last(), Some(&ContentPart::Text { text: "do it differently".into() }));
        assert_eq!(m[2].content.len(), 2, "one result, then the prompt");
    }

    #[test]
    fn results_come_before_the_text_of_their_turn() {
        let mut m = vec![
            user("q"),
            call("c1"),
            Message {
                role: Role::User,
                content: vec![
                    ContentPart::Text { text: "meanwhile".into() },
                    ContentPart::ToolResult { tool_use_id: "c1".into(), tool_name: None, content: "x".into(), is_error: false },
                ],
            },
        ];
        assert_eq!(repair_history(&mut m), 0);
        // A second call in the same turn puts its result ahead of the text too.
        let mut m2 = vec![
            user("q"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::ToolUse { id: "c1".into(), name: "read_file".into(), input: json!({}) },
                    ContentPart::ToolUse { id: "c2".into(), name: "read_file".into(), input: json!({}) },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentPart::ToolResult { tool_use_id: "c1".into(), tool_name: None, content: "x".into(), is_error: false },
                    ContentPart::Text { text: "meanwhile".into() },
                ],
            },
        ];
        assert_eq!(repair_history(&mut m2), 1);
        assert_eq!(ids(&m2[2]), ["c1", "c2"]);
        assert_eq!(m2[2].content.last(), Some(&ContentPart::Text { text: "meanwhile".into() }));
    }

    #[test]
    fn a_result_without_its_call_becomes_text() {
        let mut m = vec![user("q"), result("gone"), user("next")];
        assert_eq!(repair_history(&mut m), 1);
        assert!(ids(&m[1]).is_empty());
        assert_eq!(m[1].content, vec![ContentPart::Text { text: "gone returned: x".into() }]);
    }

    #[test]
    fn images_are_checked() {
        let im = |t: &str, d: &str| ImageInput { media_type: t.into(), data: d.into() };
        assert_eq!(image_parts(&[im("image/png", "iVBORw0KGgo=")]).unwrap().len(), 1);
        assert!(image_parts(&[im("image/svg+xml", "PHN2Zz4=")]).is_err(), "svg is not an image the models read");
        assert!(image_parts(&[im("image/png", "data:image/png;base64,xx")]).is_err(), "no data: prefix");
        assert!(image_parts(&[im("image/png", "")]).is_err());
        assert!(image_parts(&vec![im("image/png", "AA=="); MAX_IMAGES + 1]).is_err());
    }

    #[test]
    fn a_call_between_assistant_turns_gets_its_own_turn() {
        let mut m = vec![user("q"), call("c1"), call("c2"), user("next")];
        assert_eq!(repair_history(&mut m), 2, "one answer each");
        assert_eq!(m.len(), 5);
        assert_eq!(ids(&m[2]), ["c1"], "the answer sits right after its call");
        assert_eq!(ids(&m[4]), ["c2"]);
        assert_eq!(repair_history(&mut m), 0, "a second pass finds nothing to do");
    }
}
