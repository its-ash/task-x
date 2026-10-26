//! End-to-end tests: spawn the runtime binary, speak JSON-RPC over stdio, and point it at a
//! local mock provider. No network access or paid API keys required.

use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(10);

struct Runtime {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Runtime {
    async fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_task-x-runtime"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self { child, stdin, stdout }
    }

    async fn send(&mut self, v: Value) {
        self.stdin.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(WAIT, self.stdout.next_line()).await.expect("timed out").unwrap().expect("eof");
        serde_json::from_str(&line).unwrap()
    }

    /// Reads until the response for `id`, collecting notifications seen on the way.
    async fn until_response(&mut self, id: i64) -> (Value, Vec<Value>) {
        let mut notes = Vec::new();
        loop {
            let m = self.recv().await;
            if m["id"] == id {
                return (m, notes);
            }
            notes.push(m);
        }
    }

    async fn init() -> Self {
        let mut rt = Self::start().await;
        rt.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"test"}}})).await;
        let r = rt.recv().await;
        assert_eq!(r["result"]["protocolVersion"], 1);
        rt
    }
}

/// Serves one canned HTTP response per connection; each body chunk is flushed separately.
/// `hang` keeps the connection open after the chunks. Captured request heads are sent on `seen`.
async fn mock(status: u16, content_type: &str, chunks: Vec<String>, hang: bool) -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let ct = content_type.to_owned();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (tx, ct, chunks) = (tx.clone(), ct.clone(), chunks.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len = text[..i]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            let _ = tx.send(text);
                            break;
                        }
                    }
                }
                let head = format!("HTTP/1.1 {status} X\r\ncontent-type: {ct}\r\nconnection: close\r\n\r\n");
                sock.write_all(head.as_bytes()).await.unwrap();
                for c in chunks {
                    if sock.write_all(c.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = sock.flush().await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                if hang {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            });
        }
    });
    (format!("http://{addr}/v1"), rx)
}

fn chat(id: i64, base: &str, key: Option<&str>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"chat/send","params":{
        "provider":{"kind":"openaiCompatible","model":"mock","baseUrl":base,"apiKey":key,"timeoutMs":5000},
        "request":{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}
    }})
}

#[tokio::test]
async fn streams_chat_events_then_returns_aggregated_result() {
    let sse = [
        r#"{"choices":[{"index":0,"delta":{"content":"Hel"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
    ]
    .iter()
    .map(|d| format!("data: {d}\n\n"))
    .chain(["data: [DONE]\n\n".to_owned()])
    .collect();
    let (base, mut seen) = mock(200, "text/event-stream", sse, false).await;
    let mut rt = Runtime::init().await;
    rt.send(chat(1, &base, Some("sk-local"))).await;
    let (res, notes) = rt.until_response(1).await;

    assert_eq!(res["result"]["text"], "Hello");
    assert_eq!(res["result"]["stopReason"], "endTurn");
    let deltas: Vec<&str> = notes
        .iter()
        .filter(|n| n["params"]["event"]["type"] == "textDelta")
        .map(|n| n["params"]["event"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, ["Hel", "lo"]);
    assert!(notes.iter().all(|n| n["method"] == "chat/event" && n["params"]["requestId"] == 1));

    let req = seen.recv().await.unwrap();
    assert!(req.starts_with("POST /v1/chat/completions"));
    assert!(req.to_ascii_lowercase().contains("authorization: bearer sk-local"));
}

#[tokio::test]
async fn refine_rewrites_the_prompt_with_the_routed_model() {
    let reply = json!({ "choices": [{ "index": 0, "delta": { "content": "<think>plan</think>```text\nAdd a test for parse() in src/lib.rs.\n```" }, "finish_reason": "stop" }] });
    let (base, mut seen) = mock(200, "text/event-stream", vec![format!("data: {reply}\n\n"), "data: [DONE]\n\n".into()], false).await;
    let kinds = ["question", "bugInvestigation", "codeModification", "newFeature", "refactoring", "testing", "documentation", "repositoryWide", "architecture"];
    let mut rt = Runtime::init().await;
    rt.send(json!({"jsonrpc":"2.0","id":1,"method":"prompt/refine","params":{
        "prompt": "test parse",
        "provider": {"kind":"openaiCompatible","model":"main","baseUrl":"http://127.0.0.1:1/v1","apiKey":"k","timeoutMs":5000},
        "routes": [{ "kinds": kinds, "provider": {"kind":"openaiCompatible","model":"routed","baseUrl":base,"apiKey":"k","timeoutMs":5000} }]
    }}))
    .await;
    let (res, _) = rt.until_response(1).await;
    assert_eq!(res["result"]["text"], "Add a test for parse() in src/lib.rs.", "{res}");
    assert_eq!((res["result"]["model"].as_str(), res["result"]["routed"].as_bool()), (Some("routed"), Some(true)));
    assert!(seen.recv().await.unwrap().contains("test parse"), "the prompt goes to the routed model");
    rt.send(json!({"jsonrpc":"2.0","id":2,"method":"prompt/refine","params":{ "prompt": "test parse" }})).await;
    assert_eq!(rt.until_response(2).await.0["error"]["code"], -32602, "no route and no provider");
    rt.send(json!({"jsonrpc":"2.0","id":3,"method":"prompt/refine","params":{ "prompt": "  " }})).await;
    assert_eq!(rt.until_response(3).await.0["error"]["code"], -32602);
}

#[tokio::test]
async fn cancel_request_aborts_in_flight_stream() {
    let first = format!("data: {}\n\n", r#"{"choices":[{"index":0,"delta":{"content":"partial"}}]}"#);
    let (base, _seen) = mock(200, "text/event-stream", vec![first], true).await;
    let mut rt = Runtime::init().await;
    rt.send(chat(2, &base, Some("k"))).await;
    let note = rt.recv().await;
    assert_eq!(note["params"]["event"]["text"], "partial");
    rt.send(json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":2}})).await;
    let (res, _) = rt.until_response(2).await;
    assert_eq!(res["error"]["code"], -32800);
    assert_eq!(res["error"]["data"]["kind"], "cancelled");
}

#[tokio::test]
async fn http_errors_are_normalized() {
    let (base, _seen) = mock(401, "application/json", vec![r#"{"error":{"message":"bad key"}}"#.into()], false).await;
    let mut rt = Runtime::init().await;
    rt.send(chat(3, &base, Some("wrong"))).await;
    let (res, _) = rt.until_response(3).await;
    assert_eq!(res["error"]["code"], -32001);
    assert_eq!(res["error"]["message"], "bad key");
    assert_eq!(res["error"]["data"], json!({"kind":"auth","message":"bad key","status":401,"retryable":false}));
}

#[tokio::test]
async fn provider_test_reports_models_and_availability() {
    let (base, _seen) = mock(200, "application/json", vec![r#"{"data":[{"id":"mock"},{"id":"other"}]}"#.into()], false).await;
    let mut rt = Runtime::init().await;
    rt.send(json!({"jsonrpc":"2.0","id":4,"method":"provider/test","params":{"provider":{"kind":"openaiCompatible","model":"mock","baseUrl":base,"apiKey":"k"}}})).await;
    let (res, _) = rt.until_response(4).await;
    assert_eq!(res["result"]["models"], json!(["mock", "other"]));
    assert_eq!(res["result"]["modelAvailable"], true);
    assert_eq!(res["result"]["capabilities"]["messageFormat"], "openAiChat");
}

#[tokio::test]
async fn provider_context_window_reads_model_metadata() {
    let (base, _seen) = mock(200, "application/json", vec![r#"{"data":[{"id":"mock","context_length":65536}]}"#.into()], false).await;
    let mut rt = Runtime::init().await;
    rt.send(json!({"jsonrpc":"2.0","id":5,"method":"provider/contextWindow","params":{"provider":{"kind":"openaiCompatible","model":"mock","baseUrl":base,"apiKey":"k"}}})).await;
    let (res, _) = rt.until_response(5).await;
    assert_eq!(res["result"]["tokens"], 65536);
}

#[tokio::test]
async fn protocol_errors_and_lifecycle() {
    let mut rt = Runtime::start().await;
    rt.send(json!({"jsonrpc":"2.0","id":1,"method":"chat/send","params":{}})).await;
    assert_eq!(rt.recv().await["error"]["code"], -32003);

    rt.send(json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":99}})).await;
    let r = rt.recv().await;
    assert_eq!((r["error"]["code"].clone(), r["error"]["data"]["serverProtocolVersion"].clone()), (json!(-32002), json!(1)));

    rt.send(json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":1}})).await;
    assert_eq!(rt.recv().await["result"]["providers"].as_array().unwrap().len(), 4);

    rt.stdin.write_all(b"{not json\n").await.unwrap();
    assert_eq!(rt.recv().await["error"]["code"], -32700);

    rt.send(json!({"jsonrpc":"2.0","id":4,"method":"nope"})).await;
    assert_eq!(rt.recv().await["error"]["code"], -32601);

    rt.send(json!({"jsonrpc":"2.0","id":5,"method":"chat/send","params":{"provider":{"kind":"anthropic","model":"m"},"request":{"messages":[]}}})).await;
    let r = rt.recv().await;
    assert_eq!(r["error"]["data"]["kind"], "auth");

    rt.send(json!({"jsonrpc":"2.0","id":6,"method":"shutdown"})).await;
    assert_eq!(rt.recv().await["result"], Value::Null);
    rt.send(json!({"jsonrpc":"2.0","method":"exit"})).await;
    let status = tokio::time::timeout(WAIT, rt.child.wait()).await.unwrap().unwrap();
    assert!(status.success());
}

/// A runtime that was killed mid-run leaves `status: "running"` behind; the next start must clear it,
/// or the task list and the status bar show work that is not happening.
#[tokio::test]
async fn interrupted_tasks_are_marked_failed_on_start() {
    let dir = std::env::temp_dir().join(format!("taskx-stale-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("tasks")).unwrap();
    let stale = json!({
        "version": 1, "id": "stale1", "title": "left running", "createdMs": 1, "updatedMs": 2,
        "status": "running", "chatMode": "agent", "providerKind": "OpenAiCompatible", "model": "m",
        "system": "s", "messages": [], "usage": { "inputTokens": null, "outputTokens": null },
        "iterations": 1, "toolCalls": 0, "changedFiles": [], "operations": [], "validations": [],
        "finalText": "", "error": null
    });
    std::fs::write(dir.join("tasks/stale1.json"), serde_json::to_vec(&stale).unwrap()).unwrap();

    let mut rt = Runtime::start().await;
    rt.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"test"},"dataDir":dir}})).await;
    assert_eq!(rt.recv().await["result"]["protocolVersion"], 1);
    rt.send(json!({"jsonrpc":"2.0","id":1,"method":"agent/tasks"})).await;
    let (res, _) = rt.until_response(1).await;
    let task = res["result"].as_array().unwrap().iter().find(|t| t["id"] == "stale1").expect("the record is listed");
    assert_eq!(task["status"], "failed", "{task}");
    std::fs::remove_dir_all(dir).ok();
}
