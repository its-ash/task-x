use super::HttpRequest;
use super::stream::{SseParser, StreamDecoder};
use super::types::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const API_VERSION: &str = "2023-06-01";

fn headers(cfg: &ResolvedConfig) -> Vec<(&'static str, String)> {
    let mut h = vec![("anthropic-version", API_VERSION.to_owned())];
    if let Some(k) = &cfg.api_key {
        h.push(("x-api-key", k.clone()));
    }
    h
}

pub fn chat_request(cfg: &ResolvedConfig, req: &ChatRequest) -> HttpRequest {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let content: Vec<Value> = m
                .content
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => json!({ "type": "text", "text": text }),
                    ContentPart::ToolUse { id, name, input } => json!({ "type": "tool_use", "id": id, "name": name, "input": input }),
                    ContentPart::ToolResult { tool_use_id, content, is_error, .. } => {
                        json!({ "type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error })
                    }
                    ContentPart::Image { media_type, data } => {
                        json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } })
                    }
                })
                .collect();
            json!({ "role": m.role, "content": content })
        })
        .collect();
    let mut body = json!({ "model": cfg.model, "max_tokens": req.max_tokens(), "messages": messages, "stream": true });
    if let Some(s) = req.system.as_deref().filter(|s| !s.is_empty()) {
        body["system"] = json!(s);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if !req.tools.is_empty() {
        body["tools"] =
            req.tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema })).collect();
    }
    HttpRequest::post(cfg.endpoint("/v1/messages"), headers(cfg), body)
}

/// Context window in tokens: 1M for Claude 4.6 and later (Haiku 4.5: 200K), 200K for older models.
pub fn context_window(model: &str) -> usize {
    let m = model.trim_start_matches("anthropic.");
    let large = [
        "claude-fable",
        "claude-mythos",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-haiku-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-4-6",
    ];
    if large.iter().any(|p| m.starts_with(p)) { 1_000_000 } else { 200_000 }
}

pub fn models_request(cfg: &ResolvedConfig) -> HttpRequest {
    HttpRequest::get(cfg.endpoint("/v1/models?limit=1000"), headers(cfg))
}

pub fn parse_models(v: &Value) -> Vec<String> {
    v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect()
}

/// The input window the models API reports for one model (`max_input_tokens`).
pub fn parse_context_window(model: &str, v: &Value) -> Option<usize> {
    v["data"].as_array()?.iter().find(|m| m["id"].as_str() == Some(model))?["max_input_tokens"].as_u64().map(|n| n as usize)
}

struct ToolBlock {
    id: String,
    name: String,
    json: String,
}

#[derive(Default)]
pub struct Decoder {
    sse: SseParser,
    tools: BTreeMap<u64, ToolBlock>,
    stop: Option<StopReason>,
    done: bool,
}

impl Decoder {
    fn handle(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let v: Value = serde_json::from_str(data).map_err(|e| ProviderError::protocol(format!("invalid Anthropic event: {e}")))?;
        let idx = v["index"].as_u64().unwrap_or(0);
        let mut out = Vec::new();
        match v["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let u = &v["message"]["usage"];
                out.push(StreamEvent::Usage {
                    usage: Usage { input_tokens: u["input_tokens"].as_u64(), output_tokens: u["output_tokens"].as_u64() },
                });
            }
            "content_block_start" => {
                let b = &v["content_block"];
                match b["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = b["text"].as_str().filter(|t| !t.is_empty()) {
                            out.push(StreamEvent::TextDelta { text: t.to_owned() });
                        }
                    }
                    Some("tool_use") => {
                        let id = b["id"].as_str().unwrap_or_default().to_owned();
                        let name = b["name"].as_str().unwrap_or_default().to_owned();
                        self.tools.insert(idx, ToolBlock { id, name, json: String::new() });
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(t) = d["text"].as_str() {
                            out.push(StreamEvent::TextDelta { text: t.to_owned() });
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(tb), Some(p)) = (self.tools.get_mut(&idx), d["partial_json"].as_str()) {
                            tb.json.push_str(p);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(tb) = self.tools.remove(&idx) {
                    out.push(finish_tool(tb)?);
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(StopReason::from_provider(r));
                }
                if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                    out.push(StreamEvent::Usage {
                        usage: Usage { input_tokens: v["usage"]["input_tokens"].as_u64(), output_tokens: Some(o) },
                    });
                }
            }
            "message_stop" => {
                self.done = true;
                out.push(StreamEvent::Done { stop_reason: self.stop.unwrap_or_default() });
            }
            "error" => {
                let e = &v["error"];
                let msg = e["message"].as_str().unwrap_or("provider stream error");
                let kind = match e["type"].as_str() {
                    Some("overloaded_error") => ErrorKind::Overloaded,
                    Some("rate_limit_error") => ErrorKind::RateLimited,
                    Some("api_error") => ErrorKind::Server,
                    Some("authentication_error") => ErrorKind::Auth,
                    Some("invalid_request_error") => ErrorKind::InvalidRequest,
                    _ => ErrorKind::Protocol,
                };
                return Err(ProviderError::new(kind, msg));
            }
            _ => {}
        }
        Ok(out)
    }
}

fn finish_tool(tb: ToolBlock) -> Result<StreamEvent, ProviderError> {
    let input = if tb.json.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&tb.json).map_err(|e| ProviderError::protocol(format!("invalid tool input JSON for '{}': {e}", tb.name)))?
    };
    Ok(StreamEvent::ToolCall { id: tb.id, name: tb.name, input })
}

impl StreamDecoder for Decoder {
    fn feed_line(&mut self, line: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        match self.sse.feed_line(line) {
            Some(ev) if !self.done => self.handle(&ev.data),
            _ => Ok(Vec::new()),
        }
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        let out = match self.sse.flush() {
            Some(ev) if !self.done => self.handle(&ev.data)?,
            _ => Vec::new(),
        };
        if !self.done {
            return Err(ProviderError::new(ErrorKind::Network, "stream ended before message_stop"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ResolvedConfig {
        ProviderConfig {
            kind: ProviderKind::Anthropic,
            model: "claude-test".into(),
            base_url: None,
            api_key: Some("sk-test".into()),
            timeout_ms: None,
        }
        .resolve()
        .unwrap()
    }

    #[test]
    fn sends_attached_images_as_base64_blocks() {
        let img = ContentPart::Image { media_type: "image/png".into(), data: "iVBORw0KGgo=".into() };
        let req = ChatRequest {
            messages: vec![Message { role: Role::User, content: vec![ContentPart::Text { text: "what is this".into() }, img] }],
            ..Default::default()
        };
        let b = chat_request(&cfg(), &req).body.unwrap();
        assert_eq!(
            b["messages"][0]["content"][1],
            json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" } })
        );
    }

    #[test]
    fn builds_messages_request_with_tools_and_tool_results() {
        let req = ChatRequest {
            system: Some("be brief".into()),
            messages: vec![
                Message { role: Role::User, content: vec![ContentPart::Text { text: "hi".into() }] },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolUse { id: "t1".into(), name: "read_file".into(), input: json!({"path": "a"}) }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentPart::ToolResult {
                        tool_use_id: "t1".into(),
                        tool_name: None,
                        content: "x".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSpec { name: "read_file".into(), description: "read".into(), input_schema: json!({"type": "object"}) }],
            max_tokens: Some(100),
            temperature: None,
            context_window: None,
        };
        let r = chat_request(&cfg(), &req);
        assert_eq!(r.url, "https://api.anthropic.com/v1/messages");
        assert!(r.headers.iter().any(|(k, v)| *k == "x-api-key" && v == "sk-test"));
        let b = r.body.unwrap();
        assert_eq!(b["system"], "be brief");
        assert_eq!(b["max_tokens"], 100);
        assert_eq!(b["stream"], true);
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(b["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(b["messages"][2]["content"][0]["tool_use_id"], "t1");
        assert!(b.get("temperature").is_none());
    }

    fn feed(d: &mut Decoder, raw: &str) -> Vec<StreamEvent> {
        raw.lines().flat_map(|l| d.feed_line(l).unwrap()).collect()
    }

    #[test]
    fn decodes_text_tool_use_usage_and_stop() {
        let raw = r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}

data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"search","input":{}}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"x\"}"}}

data: {"type":"content_block_stop","index":1}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}

data: {"type":"message_stop"}

"#;
        let mut d = Decoder::default();
        let mut out = ChatOutcome::default();
        for e in feed(&mut d, raw).iter().chain(d.finish().unwrap().iter()) {
            out.apply(e);
        }
        assert_eq!(out.text, "Hello");
        assert_eq!(out.tool_calls[0].name, "search");
        assert_eq!(out.tool_calls[0].input, json!({"q": "x"}));
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(out.usage, Usage { input_tokens: Some(12), output_tokens: Some(30) });
    }

    #[test]
    fn maps_stream_error_event() {
        let mut d = Decoder::default();
        d.feed_line(r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#).unwrap();
        let e = d.feed_line("").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Overloaded);
        assert!(e.retryable);
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let mut d = Decoder::default();
        feed(&mut d, "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\"}}\n\n");
        assert_eq!(d.finish().unwrap_err().kind, ErrorKind::Network);
    }
}
