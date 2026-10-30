use super::HttpRequest;
use super::openai::tool_specs;
use super::stream::StreamDecoder;
use super::types::*;
use serde_json::{Value, json};

fn headers(cfg: &ResolvedConfig) -> Vec<(&'static str, String)> {
    cfg.api_key.iter().map(|k| ("authorization", format!("Bearer {k}"))).collect()
}

fn messages(req: &ChatRequest) -> Vec<Value> {
    let mut out = Vec::with_capacity(req.messages.len() + 1);
    if let Some(s) = req.system.as_deref().filter(|s| !s.is_empty()) {
        out.push(json!({ "role": "system", "content": s }));
    }
    for m in &req.messages {
        let calls: Vec<Value> = m
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolUse { name, input, .. } => Some(json!({ "function": { "name": name, "arguments": input } })),
                _ => None,
            })
            .collect();
        for p in &m.content {
            if let ContentPart::ToolResult { content, tool_name, .. } = p {
                let mut t = json!({ "role": "tool", "content": content });
                if let Some(n) = tool_name {
                    t["tool_name"] = json!(n);
                }
                out.push(t);
            }
        }
        let text = m.text();
        let images: Vec<&str> = m
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Image { data, .. } => Some(data.as_str()),
                _ => None,
            })
            .collect();
        if !calls.is_empty() {
            out.push(json!({ "role": "assistant", "content": text, "tool_calls": calls }));
        } else if !images.is_empty() {
            out.push(json!({ "role": m.role, "content": text, "images": images }));
        } else if !text.is_empty() {
            out.push(json!({ "role": m.role, "content": text }));
        }
    }
    out
}

pub fn chat_request(cfg: &ResolvedConfig, req: &ChatRequest) -> HttpRequest {
    let mut options = json!({ "num_predict": req.max_tokens() });
    // Without num_ctx, Ollama uses its small default window and silently cuts long prompts.
    if let Some(n) = req.context_window {
        options["num_ctx"] = json!(n);
    }
    if let Some(t) = req.temperature {
        options["temperature"] = json!(t);
    }
    let mut body = json!({ "model": cfg.model, "messages": messages(req), "stream": true, "options": options });
    // gpt-oss takes a thinking level; other models only on or off, so they keep their default.
    if let Some(e) = req.effort.filter(|_| cfg.model.starts_with("gpt-oss")) {
        body["think"] = json!(e.three_level());
    }
    if !req.tools.is_empty() {
        body["tools"] = json!(tool_specs(&req.tools));
    }
    HttpRequest::post(cfg.endpoint("/api/chat"), headers(cfg), body)
}

/// Window the agent asks Ollama for (`num_ctx`). Larger windows need more memory on the user's machine.
pub const DEFAULT_CONTEXT_WINDOW: usize = 32_768;

/// `POST /api/embed` for a batch of texts (an embedding model such as nomic-embed-text).
pub fn embed_request(cfg: &ResolvedConfig, texts: &[String]) -> HttpRequest {
    HttpRequest::post(cfg.endpoint("/api/embed"), headers(cfg), json!({ "model": cfg.model, "input": texts }))
}

pub fn parse_embeddings(v: &Value) -> Vec<Vec<f32>> {
    v["embeddings"].as_array().into_iter().flatten().map(|e| e.as_array().into_iter().flatten().filter_map(|x| x.as_f64().map(|f| f as f32)).collect()).collect()
}

pub fn models_request(cfg: &ResolvedConfig) -> HttpRequest {
    HttpRequest::get(cfg.endpoint("/api/tags"), headers(cfg))
}

pub fn parse_models(v: &Value) -> Vec<String> {
    v["models"].as_array().into_iter().flatten().filter_map(|m| m["name"].as_str().map(str::to_owned)).collect()
}

pub fn show_request(cfg: &ResolvedConfig) -> HttpRequest {
    HttpRequest::post(cfg.endpoint("/api/show"), headers(cfg), json!({ "model": cfg.model }))
}

/// `/api/show` for one named model.
pub fn show_model_request(cfg: &ResolvedConfig, model: &str) -> HttpRequest {
    HttpRequest::post(cfg.endpoint("/api/show"), headers(cfg), json!({ "model": model }))
}

/// One installed model: its `/api/tags` entry, and its `/api/show` answer when that worked.
pub fn model_info(name: &str, tag: &Value, show: Option<&Value>) -> ModelInfo {
    // `ModelInfo::new` keeps the known capabilities (Ollama also lists `completion` and `insert`).
    let caps: Vec<&str> = show.and_then(|s| s["capabilities"].as_array()).into_iter().flatten().filter_map(Value::as_str).collect();
    let mut info = ModelInfo::new(name, show.and_then(parse_context_window), &caps);
    let d = &tag["details"];
    let details: Vec<&str> = [d["parameter_size"].as_str(), d["quantization_level"].as_str()].into_iter().flatten().collect();
    info.details = (!details.is_empty()).then(|| details.join(" · "));
    info
}

/// The model's own context length from `/api/show` (`model_info.<arch>.context_length`).
pub fn parse_context_window(v: &Value) -> Option<usize> {
    v["model_info"].as_object()?.iter().find(|(k, _)| k.ends_with(".context_length")).and_then(|(_, n)| n.as_u64()).map(|n| n as usize)
}

/// Refines static capabilities using `/api/show`'s `capabilities` list (Ollama ≥ 0.6).
pub fn refine_capabilities(mut caps: Capabilities, show: &Value) -> Capabilities {
    if let Some(list) = show["capabilities"].as_array() {
        caps.tool_calling = list.iter().any(|c| c == "tools");
    }
    caps
}

/// Ollama streams newline-delimited JSON objects rather than SSE.
#[derive(Default)]
pub struct Decoder {
    calls: usize,
    done: bool,
    /// Whether any reply text came, and the `thinking` text of models that put their whole answer
    /// there: it becomes the reply when nothing else came.
    text_seen: bool,
    thinking: String,
}

/// Thinking kept for the fallback reply; more is cut.
const MAX_THINKING: usize = 60_000;

impl StreamDecoder for Decoder {
    fn feed_line(&mut self, line: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        if line.trim().is_empty() || self.done {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(line).map_err(|e| ProviderError::protocol(format!("invalid Ollama chunk: {e}")))?;
        if let Some(e) = v["error"].as_str() {
            return Err(ProviderError::new(ErrorKind::Server, e));
        }
        let mut out = Vec::new();
        let msg = &v["message"];
        if let Some(t) = msg["content"].as_str().filter(|t| !t.is_empty()) {
            self.text_seen = true;
            out.push(StreamEvent::TextDelta { text: t.to_owned() });
        }
        if let Some(t) = msg["thinking"].as_str().filter(|_| self.thinking.len() < MAX_THINKING) {
            self.thinking.push_str(t);
        }
        for tc in msg["tool_calls"].as_array().into_iter().flatten() {
            let f = &tc["function"];
            let input = match &f["arguments"] {
                Value::String(s) => serde_json::from_str(s).map_err(|e| ProviderError::protocol(format!("invalid tool arguments: {e}")))?,
                Value::Null => json!({}),
                other => other.clone(),
            };
            let id = tc["id"].as_str().map_or_else(|| format!("call_{}", self.calls), str::to_owned);
            self.calls += 1;
            out.push(StreamEvent::ToolCall { id, name: f["name"].as_str().unwrap_or_default().to_owned(), input });
        }
        if v["done"].as_bool() == Some(true) {
            self.done = true;
            // No reply text and no call: the answer was in the thinking, so that is the reply.
            if !self.text_seen && self.calls == 0 && !self.thinking.trim().is_empty() {
                out.push(StreamEvent::TextDelta { text: std::mem::take(&mut self.thinking).trim().to_owned() });
            }
            out.push(StreamEvent::Usage {
                usage: Usage {
                    input_tokens: v["prompt_eval_count"].as_u64(),
                    output_tokens: v["eval_count"].as_u64(),
                    cached_tokens: None,
                },
            });
            let stop = match v["done_reason"].as_str() {
                _ if self.calls > 0 => StopReason::ToolUse,
                Some(r) => StopReason::from_provider(r),
                None => StopReason::EndTurn,
            };
            out.push(StreamEvent::Done { stop_reason: stop });
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.done { Ok(Vec::new()) } else { Err(ProviderError::new(ErrorKind::Network, "stream ended before done=true")) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_attached_images_as_a_list() {
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    ContentPart::Text { text: "q".into() },
                    ContentPart::Image { media_type: "image/png".into(), data: "AA==".into() },
                ],
            }],
            ..Default::default()
        };
        assert_eq!(messages(&req)[0], json!({ "role": "user", "content": "q", "images": ["AA=="] }));
    }

    #[test]
    fn builds_chat_request_without_key_against_default_host() {
        let cfg = ProviderConfig { kind: ProviderKind::Ollama, model: "qwen3".into(), base_url: None, api_key: None, timeout_ms: None }
            .resolve()
            .unwrap();
        let req = ChatRequest {
            system: Some("s".into()),
            messages: vec![
                Message { role: Role::User, content: vec![ContentPart::Text { text: "q".into() }] },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolUse { id: "x".into(), name: "f".into(), input: json!({"a": 1}) }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentPart::ToolResult {
                        tool_use_id: "x".into(),
                        tool_name: Some("f".into()),
                        content: "r".into(),
                        is_error: false,
                    }],
                },
            ],
            temperature: Some(0.2),
            context_window: Some(32_768),
            ..Default::default()
        };
        let r = chat_request(&cfg, &req);
        assert_eq!(r.url, "http://127.0.0.1:11434/api/chat");
        assert!(r.headers.is_empty());
        let b = r.body.unwrap();
        assert_eq!(b["messages"][2]["tool_calls"][0]["function"]["arguments"]["a"], 1);
        assert_eq!(b["messages"][3]["tool_name"], "f");
        assert!((b["options"]["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        assert_eq!(b["options"]["num_ctx"], 32_768, "the window is asked for, not Ollama's small default");
    }

    #[test]
    fn decodes_ndjson_stream_with_tool_call() {
        let mut d = Decoder::default();
        let mut out = ChatOutcome::default();
        for l in [
            r#"{"message":{"role":"assistant","content":"ok"},"done":false}"#,
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"ls","arguments":{"dir":"."}}}]},"done":false}"#,
            r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":4}"#,
        ] {
            d.feed_line(l).unwrap().iter().for_each(|e| out.apply(e));
        }
        assert!(d.finish().unwrap().is_empty());
        assert_eq!(out.text, "ok");
        assert_eq!(out.tool_calls[0].id, "call_0");
        assert_eq!(out.tool_calls[0].input, json!({"dir": "."}));
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(out.usage, Usage { input_tokens: Some(3), output_tokens: Some(4), cached_tokens: None });
    }

    /// A model that answers only in `thinking` still gets a reply; with any content, thinking stays out.
    #[test]
    fn an_answer_only_in_thinking_becomes_the_reply() {
        let run = |lines: &[&str]| {
            let mut d = Decoder::default();
            let mut out = ChatOutcome::default();
            for l in lines {
                d.feed_line(l).unwrap().iter().for_each(|e| out.apply(e));
            }
            out.text
        };
        let only = [r#"{"message":{"content":"","thinking":"Missing: color, "},"done":false}"#, r#"{"message":{"content":"","thinking":"config."},"done":true}"#];
        assert_eq!(run(&only), "Missing: color, config.");
        let both = [r#"{"message":{"content":"Answer","thinking":"why"},"done":false}"#, r#"{"message":{"content":""},"done":true}"#];
        assert_eq!(run(&both), "Answer");
    }

    #[test]
    fn surfaces_inline_errors_and_capabilities() {
        let mut d = Decoder::default();
        assert_eq!(d.feed_line(r#"{"error":"model not found"}"#).unwrap_err().message, "model not found");
        let caps = refine_capabilities(ProviderKind::Ollama.capabilities(), &json!({"capabilities": ["completion"]}));
        assert!(!caps.tool_calling);
    }
}
