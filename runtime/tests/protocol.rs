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
        Self::start_with(&[]).await
    }

    async fn start_with(env: &[(&str, &std::path::Path)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_task-x-runtime"));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
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
    let kinds = [
        "question",
        "bugInvestigation",
        "codeModification",
        "newFeature",
        "refactoring",
        "testing",
        "documentation",
        "repositoryWide",
        "architecture",
    ];
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

/// `plugins/configure` adds a plugin folder; `plugins/list`, `agents/list`, and `skills/list` then
/// show its agents, skills, and commands, named `<plugin>:<name>`. A disabled plugin adds nothing.
#[tokio::test]
async fn plugins_bring_agents_skills_and_commands() {
    let base = std::env::temp_dir().join(format!("task-x-proto-plugins-{}", std::process::id()));
    let (home, plug) = (base.join("home"), base.join("kit"));
    std::fs::create_dir_all(home.join(".claude/agents")).unwrap();
    std::fs::write(home.join(".claude/agents/scout.md"), "---\ndescription: Finds\ntools: Read\n---\nYou are Scout.").unwrap();
    std::fs::create_dir_all(plug.join(".claude-plugin")).unwrap();
    std::fs::write(plug.join(".claude-plugin/plugin.json"), r#"{ "name": "kit", "version": "1.2.0" }"#).unwrap();
    std::fs::create_dir_all(plug.join("agents")).unwrap();
    std::fs::write(plug.join("agents/fixer.md"), "---\ndescription: Fixes\n---\nFix things.").unwrap();
    std::fs::create_dir_all(plug.join("commands")).unwrap();
    std::fs::write(plug.join("commands/ship.md"), "---\nargument-hint: [version]\n---\nShip $ARGUMENTS").unwrap();
    let mut rt = Runtime::start_with(&[("HOME", &home), ("USERPROFILE", &home)]).await;
    rt.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1}})).await;
    rt.recv().await;
    rt.send(json!({"jsonrpc":"2.0","id":1,"method":"plugins/configure","params":{"dirs":["relative/path"]}})).await;
    assert_eq!(rt.recv().await["error"]["code"], -32602, "plugin folders must be absolute");
    rt.send(json!({"jsonrpc":"2.0","id":2,"method":"plugins/configure","params":{"dirs":[plug]}})).await;
    assert_eq!(rt.recv().await["result"], json!({ "plugins": 1, "enabled": 1 }));
    rt.send(json!({"jsonrpc":"2.0","id":3,"method":"plugins/list"})).await;
    let p = rt.recv().await["result"][0].clone();
    assert_eq!((p["name"].clone(), p["version"].clone(), p["agents"].clone(), p["commands"].clone()), (json!("kit"), json!("1.2.0"), json!(1), json!(1)));
    rt.send(json!({"jsonrpc":"2.0","id":4,"method":"agents/list"})).await;
    let agents = rt.recv().await["result"].clone();
    let names: Vec<&str> = agents.as_array().unwrap().iter().filter_map(|a| a["name"].as_str()).collect();
    assert_eq!(names, ["scout", "kit:fixer", "designer"], "the user's agents, then plugin agents, then built-in ones");
    assert_eq!(agents[0]["writes"], false);
    rt.send(json!({"jsonrpc":"2.0","id":5,"method":"skills/list"})).await;
    let skills = rt.recv().await["result"].clone();
    let ship = skills.as_array().unwrap().iter().find(|s| s["name"] == "kit:ship").expect("plugin command listed").clone();
    assert_eq!((ship["kind"].clone(), ship["argumentHint"].clone()), (json!("command"), json!("[version]")));
    rt.send(json!({"jsonrpc":"2.0","id":6,"method":"skills/resolve","params":{"prompt":"/ship 2.0"}})).await;
    assert_eq!(rt.recv().await["result"]["active"][0]["text"], "Ship 2.0");
    rt.send(json!({"jsonrpc":"2.0","id":7,"method":"plugins/configure","params":{"dirs":[plug],"disabled":["kit"]}})).await;
    assert_eq!(rt.recv().await["result"]["enabled"], 0);
    rt.send(json!({"jsonrpc":"2.0","id":8,"method":"agents/list"})).await;
    assert_eq!(rt.recv().await["result"].as_array().unwrap().len(), 2, "a disabled plugin's agents are gone");
    std::fs::remove_dir_all(base).ok();
}

/// `task-x-runtime exec` runs one task against a model and exits 0, with the summary as JSON; a model
/// that cannot be reached makes it exit 1.
#[tokio::test]
async fn the_cli_runs_one_task_and_exits_with_its_status() {
    let sse = [r#"{"choices":[{"index":0,"delta":{"content":"Hello from the CLI."},"finish_reason":"stop"}]}"#, "[DONE]"];
    let (base, _) = mock(200, "text/event-stream", sse.iter().map(|d| format!("data: {d}\n\n")).collect(), false).await;
    let dir = std::env::temp_dir().join(format!("task-x-proto-cli-{}", std::process::id()));
    let (home, ws) = (dir.join("home"), dir.join("ws"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.rs"), "pub fn hi() {}\n").unwrap();
    let run = |base: String| {
        let (home, ws) = (home.clone(), ws.clone());
        async move {
            Command::new(env!("CARGO_BIN_EXE_task-x-runtime"))
                .args(["exec", "--cwd", &ws.to_string_lossy(), "--provider", "openaiCompatible", "--model", "m", "--base-url", &base, "--api-key-env", "MOCK_KEY", "--json", "say hi"])
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("MOCK_KEY", "k")
                .stdin(Stdio::null())
                .output()
                .await
                .unwrap()
        }
    };
    let out = run(base).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((summary["status"].clone(), summary["text"].clone()), (json!("completed"), json!("Hello from the CLI.")));
    assert!(home.join(".task-x/cli/tasks").read_dir().unwrap().next().is_some(), "the task is saved");
    let down = run("http://127.0.0.1:9/v1".into()).await;
    assert_eq!(down.status.code(), Some(1), "a failed task exits 1");
    std::fs::remove_dir_all(dir).ok();
}

/// The session methods on a saved task: checkpoints, cost, fork, compact, and rewind; and the hooks
/// config accepts Claude Code's event names.
#[tokio::test]
async fn session_controls_work_on_a_saved_task() {
    let dir = std::env::temp_dir().join(format!("task-x-proto-session-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("tasks")).unwrap();
    let text = |role: &str, t: &str| json!({ "role": role, "content": [{ "type": "text", "text": t }] });
    let rec = json!({
        "version": 1, "id": "s1", "title": "first", "createdMs": 1, "updatedMs": 1, "status": "completed", "chatMode": "agent",
        "providerKind": "Ollama", "model": "m", "system": "s",
        "messages": [text("user", "first"), text("assistant", "one"), text("user", "second"), text("assistant", "two")],
        "usage": { "inputTokens": 100, "outputTokens": 20 }, "iterations": 2, "toolCalls": 0, "changedFiles": [], "operations": [],
        "validations": [], "finalText": "two", "error": null,
        "checkpoints": [
            { "messages": 0, "operations": 0, "prompt": "first", "createdMs": 1 },
            { "messages": 2, "operations": 0, "prompt": "second", "createdMs": 2 }
        ]
    });
    std::fs::write(dir.join("tasks/s1.json"), rec.to_string()).unwrap();
    let mut rt = Runtime::start().await;
    rt.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"dataDir":dir}})).await;
    rt.recv().await;
    rt.send(json!({"jsonrpc":"2.0","id":1,"method":"agent/checkpoints","params":{"taskId":"s1"}})).await;
    assert_eq!(rt.recv().await["result"][1]["prompt"], "second");
    rt.send(json!({"jsonrpc":"2.0","id":2,"method":"agent/cost","params":{"taskId":"s1"}})).await;
    let c = rt.recv().await["result"].clone();
    assert_eq!((c["usage"]["inputTokens"].clone(), c["prompts"].clone()), (json!(100), json!(2)));
    rt.send(json!({"jsonrpc":"2.0","id":3,"method":"agent/fork","params":{"taskId":"s1","checkpoint":1}})).await;
    let f = rt.recv().await["result"].clone();
    assert_eq!((f["messages"].clone(), f["forkedFrom"].clone()), (json!(2), json!("s1")));
    rt.send(json!({"jsonrpc":"2.0","id":4,"method":"agent/compact","params":{"taskId":"s1"}})).await;
    assert!(rt.recv().await["result"]["beforeTokens"].is_number());
    rt.send(json!({"jsonrpc":"2.0","id":5,"method":"agent/rewind","params":{"taskId":"s1","checkpoint":1,"restoreCode":false}})).await;
    let r = rt.recv().await["result"].clone();
    assert_eq!((r["messages"].clone(), r["text"].clone()), (json!(2), json!("one")));
    rt.send(json!({"jsonrpc":"2.0","id":6,"method":"agent/task","params":{"taskId":"s1"}})).await;
    let t = rt.recv().await["result"].clone();
    assert_eq!((t["messages"].as_array().unwrap().len(), t["checkpoints"].as_array().unwrap().len()), (2, 1), "the rewind is saved");
    rt.send(json!({"jsonrpc":"2.0","id":7,"method":"agent/rewind","params":{"taskId":"s1","checkpoint":9}})).await;
    assert_eq!(rt.recv().await["error"]["code"], -32602);
    rt.send(json!({"jsonrpc":"2.0","id":8,"method":"config/hooks","params":{"hooks":[{"event":"PreToolUse","command":["true"]},{"event":"stop","command":["true"]}],"workspaceTrusted":false}})).await;
    assert_eq!(rt.recv().await["result"]["hooks"], 2, "Claude Code event names are accepted");
    rt.send(json!({"jsonrpc":"2.0","id":9,"method":"mcp/authorize","params":{"server":"nope"}})).await;
    assert_eq!(rt.recv().await["error"]["code"], -32602, "an unknown server cannot sign in");
    std::fs::remove_dir_all(dir).ok();
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
    rt.send(
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"test"},"dataDir":dir}}),
    )
    .await;
    assert_eq!(rt.recv().await["result"]["protocolVersion"], 1);
    rt.send(json!({"jsonrpc":"2.0","id":1,"method":"agent/tasks"})).await;
    let (res, _) = rt.until_response(1).await;
    let task = res["result"].as_array().unwrap().iter().find(|t| t["id"] == "stale1").expect("the record is listed");
    assert_eq!(task["status"], "failed", "{task}");
    std::fs::remove_dir_all(dir).ok();
}
