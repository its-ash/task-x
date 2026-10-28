//! OpenAI Chat Completions adapter, shared by the official API and OpenAI-compatible endpoints.

use super::HttpRequest;
use super::stream::{SseParser, StreamDecoder};
use super::types::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

fn headers(cfg: &ResolvedConfig) -> Vec<(&'static str, String)> {
    cfg.api_key.iter().map(|k| ("authorization", format!("Bearer {k}"))).collect()
}

pub fn tool_specs(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.input_schema } }))
        .collect()
}

/// Flattens internal messages into chat-completions messages: tool results become `role: tool` entries.
fn messages(req: &ChatRequest) -> Vec<Value> {
    let mut out = Vec::with_capacity(req.messages.len() + 1);
    if let Some(s) = req.system.as_deref().filter(|s| !s.is_empty()) {
        out.push(json!({ "role": "system", "content": s }));
    }
    for m in &req.messages {
        let text = m.text();
        let calls: Vec<Value> = m
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolUse { id, name, input } => {
                    Some(json!({ "id": id, "type": "function", "function": { "name": name, "arguments": input.to_string() } }))
                }
                _ => None,
            })
            .collect();
        // The assistant turn that asked for the calls comes first: chat-completions requires every
        // result to follow its `tool_calls` message directly.
        if !calls.is_empty() {
            out.push(
                json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }, "tool_calls": calls }),
            );
        }
        for p in &m.content {
            if let ContentPart::ToolResult { tool_use_id, content, .. } = p {
                out.push(json!({ "role": "tool", "tool_call_id": tool_use_id, "content": content }));
            }
        }
        let images: Vec<Value> = m
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Image { media_type, data } => {
                    Some(json!({ "type": "image_url", "image_url": { "url": format!("data:{media_type};base64,{data}") } }))
                }
                _ => None,
            })
            .collect();
        if calls.is_empty() && !images.is_empty() {
            let mut parts = if text.is_empty() { Vec::new() } else { vec![json!({ "type": "text", "text": text })] };
            parts.extend(images);
            out.push(json!({ "role": m.role, "content": parts }));
        } else if calls.is_empty() && !text.is_empty() {
            out.push(json!({ "role": m.role, "content": text }));
        }
    }
    out
}

pub fn chat_request(cfg: &ResolvedConfig, req: &ChatRequest) -> HttpRequest {
    let mut body = json!({ "model": cfg.model, "messages": messages(req), "stream": true });
    // The official API supports usage-in-stream and the newer token field; compatible servers may not.
    if cfg.kind == ProviderKind::OpenAi {
        body["stream_options"] = json!({ "include_usage": true });
        body["max_completion_tokens"] = json!(req.max_tokens());
    } else {
        body["max_tokens"] = json!(req.max_tokens());
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if !req.tools.is_empty() {
        body["tools"] = json!(tool_specs(&req.tools));
    }
    HttpRequest::post(cfg.endpoint("/chat/completions"), headers(cfg), body)
}

/// Context window in tokens. Conservative: model families with a known larger window, else 128K.
/// Users can set the exact value (`task-x.context.windowTokens`).
pub fn context_window(model: &str) -> usize {
    if model.starts_with("gpt-4.1") {
        1_000_000
    } else if model.starts_with("gpt-5") {
        272_000
    } else {
        128_000
    }
}

pub fn models_request(cfg: &ResolvedConfig) -> HttpRequest {
    HttpRequest::get(cfg.endpoint("/models"), headers(cfg))
}

pub fn parse_models(v: &Value) -> Vec<String> {
    v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect()
}

/// Every model `/models` lists, with the window and capabilities it reports. OpenRouter and
/// similar servers report `context_length`, `supported_parameters`, and input modalities; vLLM
/// reports `max_model_len`. The official API reports none, so known model families fill in.
pub fn parse_model_infos(kind: ProviderKind, v: &Value) -> Vec<ModelInfo> {
    v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let window = ["context_length", "max_context_length", "context_window", "max_model_len", "max_token_size"]
                .iter()
                .find_map(|f| m[*f].as_u64())
                .or_else(|| m["top_provider"]["context_length"].as_u64())
                .map(|n| n as usize)
                .or_else(|| (kind == ProviderKind::OpenAi).then(|| context_window(id)));
            let params: Vec<&str> = m["supported_parameters"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let mut caps = Vec::new();
            if params.contains(&"tools") {
                caps.push("tools");
            }
            if m["architecture"]["input_modalities"].as_array().is_some_and(|a| a.iter().any(|x| x == "image")) {
                caps.push("vision");
            }
            if params.iter().any(|p| matches!(*p, "reasoning" | "include_reasoning")) {
                caps.push("thinking");
            }
            if caps.is_empty() && kind == ProviderKind::OpenAi {
                caps = known_capabilities(id);
            }
            if id.contains("embedding") {
                caps = vec!["embedding"];
            }
            Some(ModelInfo::new(id, window, &caps))
        })
        .collect()
}

/// Capabilities of OpenAI model families, for the official API that does not report them.
fn known_capabilities(id: &str) -> Vec<&'static str> {
    let reasoning = ["o1", "o3", "o4", "gpt-5"].iter().any(|p| id.starts_with(p));
    if reasoning {
        vec!["tools", "vision", "thinking"]
    } else if ["gpt-4o", "gpt-4.1", "gpt-4-turbo"].iter().any(|p| id.starts_with(p)) {
        vec!["tools", "vision"]
    } else if id.starts_with("gpt-4") || id.starts_with("gpt-3.5") {
        vec!["tools"]
    } else {
        Vec::new()
    }
}

/// Extract context window from model metadata when available
pub fn parse_model_info(model_id: &str, v: &Value) -> Option<usize> {
    if let Some(data) = v["data"].as_array() {
        for model in data {
            if let Some(id) = model["id"].as_str() {
                if id == model_id {
                    // Try OpenAI-style max_token_size first
                    if let Some(max_tokens) = model.get("max_token_size").and_then(|v| v.as_u64()) {
                        return Some(max_tokens as usize);
                    }
                    // Try common metadata fields
                    for field in ["context_length", "max_context_length", "context_window"] {
                        if let Some(len) = model.get(field).and_then(|v| v.as_u64()) {
                            return Some(len as usize);
                        }
                    }
                }
            }
        }
    }
    None
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

/// Appends a name fragment, tolerating servers that repeat the whole name in every chunk.
fn merge_name(acc: &mut String, chunk: &str) {
    if acc.is_empty() {
        *acc = chunk.to_owned();
    } else if chunk == acc.as_str() || acc.starts_with(chunk) {
        // The same name again, or a prefix of it: nothing new.
    } else if let Some(rest) = chunk.strip_prefix(acc.as_str()) {
        acc.push_str(rest);
    } else {
        acc.push_str(chunk);
    }
}

/// Appends an arguments fragment. Chunks are normally deltas, but some compatible servers send the
/// whole arguments object again (or cumulatively), which used to leave `{…}{…}` — a document with
/// trailing characters that failed to parse and ended the run.
fn merge_args(acc: &mut String, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    if acc.is_empty() {
        acc.push_str(chunk);
    } else if chunk == acc.as_str() {
        // An exact repeat of everything so far.
    } else if chunk.len() > acc.len() && chunk.starts_with(acc.as_str()) {
        // The whole document again, now complete: keep the longer one.
        acc.clear();
        acc.push_str(chunk);
    } else {
        acc.push_str(chunk);
    }
}

/// Parses one call's accumulated arguments. An empty stream means "no arguments". A stream that
/// still holds two documents is read as the first complete one, so a mangled fragment costs one
/// call rather than the whole run.
fn parse_arguments(args: &str) -> Option<Value> {
    if args.trim().is_empty() {
        return Some(json!({}));
    }
    if let Ok(v) = serde_json::from_str(args) {
        return Some(v);
    }
    serde_json::Deserializer::from_str(args).into_iter::<Value>().next()?.ok()
}

#[derive(Default)]
pub struct Decoder {
    sse: SseParser,
    /// One entry per call, in the order the calls appeared.
    calls: BTreeMap<u64, PartialCall>,
    /// Wire `index` → the entry that index currently feeds. A new id at a used index starts a new
    /// call: servers that number every call 0 would otherwise have their arguments concatenated.
    slots: HashMap<u64, u64>,
    next_slot: u64,
    stop: Option<StopReason>,
    done: bool,
}

impl Decoder {
    fn handle(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        if data.trim() == "[DONE]" {
            return self.complete();
        }
        let v: Value = serde_json::from_str(data).map_err(|e| ProviderError::protocol(format!("invalid chat completion chunk: {e}")))?;
        if let Some(e) = v.get("error") {
            let msg = e["message"].as_str().or(e.as_str()).unwrap_or("provider stream error");
            return Err(ProviderError::new(ErrorKind::Server, msg));
        }
        let mut out = Vec::new();
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            out.push(StreamEvent::Usage {
                usage: Usage {
                    input_tokens: u["prompt_tokens"].as_u64(),
                    output_tokens: u["completion_tokens"].as_u64(),
                    // OpenAI caches long, repeated prompt prefixes by itself; this says how much hit.
                    cached_tokens: u["prompt_tokens_details"]["cached_tokens"].as_u64(),
                },
            });
        }
        for choice in v["choices"].as_array().into_iter().flatten().filter(|c| c["index"].as_u64().unwrap_or(0) == 0) {
            let d = &choice["delta"];
            if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
                out.push(StreamEvent::TextDelta { text: t.to_owned() });
            }
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                let index = tc["index"].as_u64().unwrap_or(0);
                let id = tc["id"].as_str().filter(|i| !i.is_empty());
                let current = self.slots.get(&index).copied();
                // A different id on an index that already answered belongs to another call.
                let restart = match (current, id) {
                    (Some(slot), Some(id)) => self.calls.get(&slot).is_some_and(|c| !c.id.is_empty() && c.id != id),
                    _ => false,
                };
                let slot = match current.filter(|_| !restart) {
                    Some(slot) => slot,
                    None => {
                        let slot = self.next_slot;
                        self.next_slot += 1;
                        self.slots.insert(index, slot);
                        slot
                    }
                };
                let pc = self.calls.entry(slot).or_default();
                if let Some(id) = id {
                    pc.id = id.to_owned();
                }
                if let Some(n) = tc["function"]["name"].as_str().filter(|n| !n.is_empty()) {
                    merge_name(&mut pc.name, n);
                }
                if let Some(a) = tc["function"]["arguments"].as_str() {
                    merge_args(&mut pc.args, a);
                }
            }
            if let Some(r) = choice["finish_reason"].as_str() {
                self.stop = Some(StopReason::from_provider(r));
            }
        }
        Ok(out)
    }

    fn complete(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;
        let mut out = Vec::new();
        for (i, pc) in std::mem::take(&mut self.calls) {
            let input = match parse_arguments(pc.args.trim()) {
                Some(v) => v,
                None => {
                    return Err(ProviderError::protocol(format!(
                        "the model streamed tool arguments for '{}' that are not valid JSON; retry the request",
                        pc.name
                    )));
                }
            };
            let id = if pc.id.is_empty() { format!("call_{i}") } else { pc.id };
            out.push(StreamEvent::ToolCall { id, name: pc.name, input });
        }
        let stop = self.stop.unwrap_or(if out.is_empty() { StopReason::EndTurn } else { StopReason::ToolUse });
        out.push(StreamEvent::Done { stop_reason: stop });
        Ok(out)
    }
}

impl StreamDecoder for Decoder {
    fn feed_line(&mut self, line: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        match self.sse.feed_line(line) {
            Some(ev) if !self.done => self.handle(&ev.data),
            _ => Ok(Vec::new()),
        }
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut out = match self.sse.flush() {
            Some(ev) if !self.done => self.handle(&ev.data)?,
            _ => Vec::new(),
        };
        // Some compatible servers close the stream after a finish_reason without sending [DONE].
        if !self.done {
            if self.stop.is_none() {
                return Err(ProviderError::new(ErrorKind::Network, "stream ended before completion"));
            }
            out.extend(self.complete()?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: ProviderKind, base: Option<&str>) -> ResolvedConfig {
        ProviderConfig { kind, model: "m".into(), base_url: base.map(Into::into), api_key: Some("k".into()), timeout_ms: None }
            .resolve()
            .unwrap()
    }

    #[test]
    fn sends_attached_images_as_data_urls() {
        let img = ContentPart::Image { media_type: "image/jpeg".into(), data: "/9j/".into() };
        let req = ChatRequest {
            messages: vec![Message { role: Role::User, content: vec![ContentPart::Text { text: "q".into() }, img] }],
            ..Default::default()
        };
        let b = chat_request(&cfg(ProviderKind::OpenAi, None), &req).body.unwrap();
        let parts = &b["messages"][0]["content"];
        assert_eq!(parts[0], json!({ "type": "text", "text": "q" }));
        assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,/9j/");
    }

    #[test]
    fn flattens_tool_turns_and_system_prompt() {
        let req = ChatRequest {
            system: Some("sys".into()),
            messages: vec![
                Message { role: Role::User, content: vec![ContentPart::Text { text: "q".into() }] },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolUse { id: "c1".into(), name: "f".into(), input: json!({"a": 1}) }],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentPart::ToolResult { tool_use_id: "c1".into(), tool_name: None, content: "r".into(), is_error: false },
                        ContentPart::Text { text: "next".into() },
                    ],
                },
            ],
            tools: vec![ToolSpec { name: "f".into(), description: "d".into(), input_schema: json!({"type": "object"}) }],
            ..Default::default()
        };
        let r = chat_request(&cfg(ProviderKind::OpenAi, None), &req);
        assert_eq!(r.url, "https://api.openai.com/v1/chat/completions");
        assert!(r.headers.contains(&("authorization", "Bearer k".into())));
        let b = r.body.unwrap();
        let roles: Vec<&str> = b["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "user"]);
        assert_eq!(b["messages"][2]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(b["messages"][3]["tool_call_id"], "c1");
        assert_eq!(b["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(b["stream_options"]["include_usage"], true);
    }

    #[test]
    fn the_assistant_tool_call_turn_comes_before_its_results() {
        // Chat-completions rejects a `tool` message that does not follow its `tool_calls` message,
        // so a turn that holds both must still be written in that order.
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Text { text: "checking".into() },
                    ContentPart::ToolUse { id: "c1".into(), name: "f".into(), input: json!({}) },
                    ContentPart::ToolResult { tool_use_id: "c1".into(), tool_name: None, content: "r".into(), is_error: false },
                ],
            }],
            ..Default::default()
        };
        let b = chat_request(&cfg(ProviderKind::OpenAi, None), &req).body.unwrap();
        let roles: Vec<&str> = b["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["assistant", "tool"]);
        assert_eq!(b["messages"][0]["content"], "checking");
        assert_eq!(b["messages"][0]["tool_calls"][0]["id"], "c1");
        assert_eq!(b["messages"][1]["tool_call_id"], "c1");
    }

    #[test]
    fn compatible_endpoint_uses_portable_fields() {
        let req = ChatRequest {
            messages: vec![Message { role: Role::User, content: vec![ContentPart::Text { text: "q".into() }] }],
            ..Default::default()
        };
        let r = chat_request(&cfg(ProviderKind::OpenAiCompatible, Some("http://localhost:8080/v1/")), &req);
        assert_eq!(r.url, "http://localhost:8080/v1/chat/completions");
        let b = r.body.unwrap();
        assert!(b.get("stream_options").is_none());
        assert_eq!(b["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn decodes_streamed_text_and_fragmented_tool_calls() {
        let lines = [
            r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"}}]}"#,
            "",
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"grep","arguments":"{\"pat"}}]}}]}"#,
            "",
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"tern\":\"x\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "",
            r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":7}}"#,
            "",
            "data: [DONE]",
            "",
        ];
        let mut d = Decoder::default();
        let mut out = ChatOutcome::default();
        for l in lines {
            d.feed_line(l).unwrap().iter().for_each(|e| out.apply(e));
        }
        d.finish().unwrap().iter().for_each(|e| out.apply(e));
        assert_eq!(out.text, "Hi");
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].id, "call_a");
        assert_eq!(out.tool_calls[0].input, json!({"pattern": "x"}));
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(out.usage.output_tokens, Some(7));
    }

    #[test]
    fn finish_without_done_marker_after_finish_reason_is_ok() {
        let mut d = Decoder::default();
        d.feed_line(r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#).unwrap();
        let evs = d.finish().unwrap();
        assert_eq!(evs, vec![StreamEvent::Done { stop_reason: StopReason::MaxTokens }]);
    }

    /// One streamed delta carrying tool-call fragments.
    fn delta(fragments: Value) -> String {
        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": fragments } }] }).to_string()
    }

    fn decode(lines: &[String]) -> ChatOutcome {
        let mut d = Decoder::default();
        let mut out = ChatOutcome::default();
        for l in lines {
            // A blank line ends each SSE event.
            for line in [format!("data: {l}"), String::new()] {
                d.feed_line(&line).unwrap().iter().for_each(|e| out.apply(e));
            }
        }
        d.finish().unwrap().iter().for_each(|e| out.apply(e));
        out
    }

    #[test]
    fn two_calls_that_share_one_index_stay_separate() {
        // Some compatible servers number every call 0. Merging them concatenated the two argument
        // objects (`{…}{…}`) and the run ended with "invalid tool arguments: trailing characters".
        let first = json!({ "command": ["sh", "-c", "ls -la /tmp/rtest/"] }).to_string();
        let second = json!({ "command": ["curl", "-s", "http://127.0.0.1:8777/v1/models"] }).to_string();
        let out = decode(&[
            delta(json!([{ "index": 0, "id": "c1", "function": { "name": "run_command", "arguments": first } }])),
            delta(json!([{ "index": 0, "id": "c2", "function": { "name": "run_command", "arguments": second } }])),
            "[DONE]".into(),
        ]);
        assert_eq!(out.tool_calls.len(), 2, "two calls, not one merged call");
        assert_eq!((out.tool_calls[0].id.as_str(), out.tool_calls[0].input["command"][0].as_str()), ("c1", Some("sh")));
        assert_eq!((out.tool_calls[1].id.as_str(), out.tool_calls[1].input["command"][0].as_str()), ("c2", Some("curl")));
    }

    #[test]
    fn arguments_sent_again_are_not_concatenated() {
        let full = json!({ "command": ["sh", "-c", "exec < /dev/tty"], "cwd": "." }).to_string();
        let cut = full.len() / 2;
        let out = decode(&[
            // A delta, then the whole document (cumulative), then the whole document again.
            delta(json!([{ "index": 0, "id": "c1", "function": { "name": "run_command", "arguments": &full[..cut] } }])),
            delta(json!([{ "index": 0, "function": { "arguments": full } }])),
            delta(json!([{ "index": 0, "function": { "arguments": full } }])),
            "[DONE]".into(),
        ]);
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].input["command"][2], "exec < /dev/tty");
    }

    #[test]
    fn a_repeated_tool_name_is_not_duplicated() {
        let out = decode(&[
            delta(json!([{ "index": 0, "id": "c1", "function": { "name": "run_command", "arguments": "{\"com" } }])),
            // The same name in every chunk is what some servers send.
            delta(json!([{ "index": 0, "function": { "name": "run_command", "arguments": "mand\":[]}" } }])),
            "[DONE]".into(),
        ]);
        assert_eq!(out.tool_calls[0].name, "run_command");
        assert_eq!(out.tool_calls[0].input, json!({ "command": [] }));
    }

    #[test]
    fn unparseable_arguments_are_read_as_far_as_they_go() {
        // The last resort when a stream still cannot be reconciled: the first complete document is
        // used, so one bad fragment costs the call it belongs to and not the whole run.
        assert_eq!(parse_arguments(r#"{"a":1}{"b":2}"#), Some(json!({ "a": 1 })));
        assert_eq!(parse_arguments(r#"{"a":1}"#), Some(json!({ "a": 1 })));
        assert_eq!(parse_arguments("   "), Some(json!({})), "a call with no arguments stays valid");
        assert_eq!(parse_arguments(r#"{"a":"#), None, "a truncated document is still an error");
    }
}
