//! The link between runtimes and the task-x Chrome extension.
//!
//! Chrome starts this binary as a native messaging host when the extension calls
//! `chrome.runtime.connectNative("com.taskx.chrome")`; Chrome passes the extension's origin
//! (`chrome-extension://<id>/`) as the first argument. The host is a hub: it speaks Chrome's
//! native messaging framing (a 4-byte length in native byte order, then JSON) on stdio, and serves
//! a Unix socket (`~/.task-x/chrome.sock`, or `TASK_X_CHROME_SOCKET`) that any number of
//! runtimes connect to. Each runtime sends one JSON request per line; the hub gives each request a
//! hub-wide id, forwards it to the extension, and routes the reply back to the runtime that asked.
//! Messages from the extension without an id are events and go to every runtime.
//!
//! Only Chrome may start the host for the extension ids in the host manifest, and the socket is
//! readable by this user only, so no token is needed. The bridge protocol is in
//! `docs/chrome-bridge.md`.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// The native messaging host name the extension connects to.
pub const HOST_NAME: &str = "com.taskx.chrome";
/// The id Chrome gives the unpacked extension in `chrome/`: fixed by the `key` in its manifest.
pub const EXTENSION_ID: &str = "ifhmkecelafiikilbgjleeopcodpnbfe";
const SOCKET_ENV: &str = "TASK_X_CHROME_SOCKET";
/// Chrome refuses a message from the host larger than 1 MB.
const MAX_TO_EXTENSION: usize = 1024 * 1024;
/// Chrome sends at most 64 MB to the host (a full-page screenshot fits).
const MAX_FROM_EXTENSION: usize = 64 * 1024 * 1024;
/// A runtime's request line: a bit under what the extension may receive.
const MAX_LINE: usize = MAX_TO_EXTENSION;

/// True when Chrome started this binary as the extension's native messaging host.
pub fn is_host_launch(first_arg: Option<&str>) -> bool {
    first_arg.is_some_and(|a| a.starts_with("chrome-extension://"))
}

/// Where the hub listens: `TASK_X_CHROME_SOCKET`, else `~/.task-x/chrome.sock`.
pub fn socket_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(SOCKET_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".task-x").join("chrome.sock"))
}

/// Reads one native messaging frame; `None` when the other side closed the stream.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Value>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_ne_bytes(len) as usize;
    if n > MAX_FROM_EXTENSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a {n}-byte message is too large")));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes one native messaging frame.
async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(v).map_err(io::Error::other)?;
    if body.len() > MAX_TO_EXTENSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the message is larger than Chrome accepts (1 MB)"));
    }
    w.write_all(&(body.len() as u32).to_ne_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

#[derive(Default)]
struct HubState {
    next_id: u64,
    next_client: u64,
    /// Hub id → (client, the id that client used).
    pending: HashMap<u64, (u64, Value)>,
    clients: HashMap<u64, mpsc::UnboundedSender<String>>,
}

type Shared = Arc<Mutex<HubState>>;

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, HubState> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs the hub until the extension closes its side: Chrome closed, or the extension reloaded.
#[cfg(unix)]
pub async fn hub<R, W>(mut ext_in: R, mut ext_out: W, socket: &Path) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A socket file left by a host that did not exit cleanly; the extension runs one host at a time.
    let _ = std::fs::remove_file(socket);
    let listener = tokio::net::UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    let ours = file_id(socket);

    let state: Shared = Arc::default();
    let (to_ext, mut ext_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(v) = ext_rx.recv().await {
            if let Err(e) = write_frame(&mut ext_out, &v).await {
                tracing::warn!("chrome host: writing to the extension failed: {e}");
                if e.kind() != io::ErrorKind::InvalidData {
                    break;
                }
            }
        }
    });
    let _ = to_ext.send(json!({ "event": "host.ready", "params": { "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id() } }));

    let accept = {
        let (state, to_ext) = (state.clone(), to_ext.clone());
        async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                tokio::spawn(serve_client(stream, state.clone(), to_ext.clone()));
            }
        }
    };
    let from_ext = {
        let state = state.clone();
        async move {
            loop {
                match read_frame(&mut ext_in).await {
                    Ok(Some(msg)) => route_from_extension(&state, msg),
                    Ok(None) => return Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::InvalidData => tracing::warn!("chrome host: a bad message: {e}"),
                    Err(e) => return Err(e),
                }
            }
        }
    };
    let res = tokio::select! {
        r = from_ext => r,
        () = accept => Ok(()),
    };
    // A newer host (the extension reconnected) may own the path by now: leave its socket alone.
    if ours.is_some() && file_id(socket) == ours {
        let _ = std::fs::remove_file(socket);
    }
    writer.abort();
    res
}

/// The device and inode of a file, to tell this host's socket from a newer one at the same path.
#[cfg(unix)]
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// A reply goes to the runtime that asked, under its own id; an event goes to every runtime.
fn route_from_extension(state: &Shared, msg: Value) {
    let mut s = lock(state);
    if let Some(hub_id) = msg.get("id").and_then(Value::as_u64) {
        let Some((client, id)) = s.pending.remove(&hub_id) else { return };
        let mut reply = msg;
        reply["id"] = id;
        if let Some(tx) = s.clients.get(&client) {
            let _ = tx.send(reply.to_string());
        }
        return;
    }
    let line = msg.to_string();
    s.clients.retain(|_, tx| tx.send(line.clone()).is_ok());
}

#[cfg(unix)]
async fn serve_client(stream: tokio::net::UnixStream, state: Shared, to_ext: mpsc::UnboundedSender<Value>) {
    let (read, mut write) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let client = {
        let mut s = lock(&state);
        s.next_client += 1;
        let c = s.next_client;
        s.clients.insert(c, tx.clone());
        c
    };
    let writer = tokio::spawn(async move {
        while let Some(mut line) = rx.recv().await {
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.len() > MAX_LINE {
            let _ = tx.send(json!({ "id": null, "error": { "message": "the request is too large" } }).to_string());
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            let _ = tx.send(json!({ "id": null, "error": { "message": "the request is not JSON" } }).to_string());
            continue;
        };
        let (Some(id), Some(method)) = (req.get("id").cloned(), req["method"].as_str()) else {
            let _ = tx.send(json!({ "id": req.get("id"), "error": { "message": "a request needs id and method" } }).to_string());
            continue;
        };
        let hub_id = {
            let mut s = lock(&state);
            s.next_id += 1;
            let h = s.next_id;
            s.pending.insert(h, (client, id));
            h
        };
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
        if to_ext.send(json!({ "id": hub_id, "method": method, "params": params, "client": client })).is_err() {
            break;
        }
    }
    let mut s = lock(&state);
    s.clients.remove(&client);
    s.pending.retain(|_, (c, _)| *c != client);
    drop(s);
    writer.abort();
}

/// Entry point when Chrome starts the binary: serve the hub on stdio until Chrome closes it.
pub async fn host_main() -> ExitCode {
    #[cfg(unix)]
    {
        let Some(path) = socket_path() else {
            eprintln!("[task-x chrome host] no HOME, so there is no place for the socket; set {SOCKET_ENV}");
            return ExitCode::FAILURE;
        };
        match hub(tokio::io::stdin(), tokio::io::stdout(), &path).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("[task-x chrome host] {e}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("[task-x chrome host] the Chrome bridge needs a Unix socket (macOS or Linux)");
        ExitCode::FAILURE
    }
}

/// A runtime's connection to the hub: requests go out one per line, replies are matched by id.
pub struct Conn {
    #[cfg(unix)]
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    #[cfg(unix)]
    write: tokio::net::unix::OwnedWriteHalf,
    next_id: u64,
}

impl Conn {
    /// The hub, when the extension is running; `None` when nothing listens on the socket.
    pub async fn connect() -> Option<Self> {
        Self::connect_to(&socket_path()?).await.ok()
    }

    #[cfg(unix)]
    pub async fn connect_to(path: &Path) -> io::Result<Self> {
        let stream = tokio::time::timeout(Duration::from_secs(2), tokio::net::UnixStream::connect(path))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the Chrome bridge did not answer"))??;
        let (read, write) = stream.into_split();
        Ok(Self { lines: BufReader::new(read).lines(), write, next_id: 1 })
    }

    #[cfg(not(unix))]
    pub async fn connect_to(_path: &Path) -> io::Result<Self> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the Chrome bridge needs a Unix socket"))
    }

    /// Sends one request and waits for its reply; events in between are skipped.
    #[cfg(unix)]
    pub async fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = json!({ "id": id, "method": method, "params": params }).to_string();
        line.push('\n');
        self.write.write_all(line.as_bytes()).await.map_err(|e| format!("{method}: the Chrome bridge closed ({e})"))?;
        let wait = async {
            loop {
                let Some(l) = self.lines.next_line().await.map_err(|e| format!("{method}: {e}"))? else {
                    return Err(format!("{method}: the Chrome extension disconnected"));
                };
                let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
                if v["id"].as_u64() != Some(id) {
                    continue;
                }
                if let Some(err) = v.get("error") {
                    return Err(format!("{method}: {}", err["message"].as_str().unwrap_or("unknown error")));
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        tokio::time::timeout(timeout, wait).await.map_err(|_| format!("{method} timed out"))?
    }

    #[cfg(not(unix))]
    pub async fn call(&mut self, method: &str, _params: Value, _timeout: Duration) -> Result<Value, String> {
        let _ = self.next_id;
        Err(format!("{method}: the Chrome bridge needs a Unix socket"))
    }
}

/// Whether the task-x Chrome extension answers (`chrome/status`): `{ connected, paused, version }`,
/// or `{ connected: false, error }`. Clients show it on their Chrome mode switch.
pub async fn status() -> Value {
    status_of(Conn::connect().await).await
}

async fn status_of(conn: Option<Conn>) -> Value {
    let Some(mut conn) = conn else {
        return json!({ "connected": false, "error": "the task-x Chrome extension is not running or not connected" });
    };
    match conn.call("ping", json!({}), Duration::from_secs(3)).await {
        Ok(v) => json!({
            "connected": true,
            "paused": v["paused"].as_bool().unwrap_or(false),
            "version": v["version"],
            "sharedTabs": v["sharedTabs"].as_array().cloned().unwrap_or_default(),
        }),
        Err(e) => json!({ "connected": false, "error": e }),
    }
}

/// The host manifest Chrome reads to find and allow the host.
fn host_manifest(binary: &Path, ids: &[String]) -> Value {
    json!({
        "name": HOST_NAME,
        "description": "task-x runtime bridge for the task-x Chrome extension",
        "path": binary,
        "type": "stdio",
        "allowed_origins": ids.iter().map(|id| format!("chrome-extension://{id}/")).collect::<Vec<_>>(),
    })
}

/// Each Chromium browser's user folder, and the `NativeMessagingHosts` folder inside it.
fn host_dirs(home: &Path) -> Vec<(&'static str, PathBuf)> {
    let browsers: &[(&str, &str)] = if cfg!(target_os = "macos") {
        &[
            ("Chrome", "Library/Application Support/Google/Chrome"),
            ("Chrome Beta", "Library/Application Support/Google/Chrome Beta"),
            ("Chrome Canary", "Library/Application Support/Google/Chrome Canary"),
            ("Chromium", "Library/Application Support/Chromium"),
            ("Edge", "Library/Application Support/Microsoft Edge"),
            ("Brave", "Library/Application Support/BraveSoftware/Brave-Browser"),
            ("Arc", "Library/Application Support/Arc/User Data"),
        ]
    } else {
        &[
            ("Chrome", ".config/google-chrome"),
            ("Chrome Beta", ".config/google-chrome-beta"),
            ("Chromium", ".config/chromium"),
            ("Edge", ".config/microsoft-edge"),
            ("Brave", ".config/BraveSoftware/Brave-Browser"),
        ]
    };
    browsers.iter().map(|(name, dir)| (*name, home.join(dir))).collect()
}

/// `task-x-runtime chrome-install [--path <binary>] [--extension-id <id>]...`: writes the host
/// manifest for every Chromium browser installed for this user, so the extension can reach the
/// runtime. `--path` defaults to this binary; extra ids are for copies of the extension loaded
/// from another folder without the manifest key.
pub fn install(args: &[String]) -> ExitCode {
    if cfg!(not(unix)) {
        eprintln!("chrome-install supports macOS and Linux; on Windows the host is registered in the registry.");
        return ExitCode::FAILURE;
    }
    let mut ids = vec![EXTENSION_ID.to_owned()];
    let mut binary = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--path" => binary = it.next().map(PathBuf::from),
            "--extension-id" => ids.extend(it.next().cloned()),
            other => {
                eprintln!("chrome-install: unknown option {other}");
                return ExitCode::FAILURE;
            }
        }
    }
    let binary = match binary.map_or_else(std::env::current_exe, Ok).and_then(std::fs::canonicalize) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("chrome-install: cannot find the runtime binary: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("chrome-install: HOME is not set");
        return ExitCode::FAILURE;
    };
    let manifest = serde_json::to_string_pretty(&host_manifest(&binary, &ids)).expect("a manifest is JSON");
    let dirs = host_dirs(&home);
    // Every installed browser gets the manifest; with none found, Chrome's folder is made for it.
    let mut targets: Vec<_> = dirs.iter().filter(|(_, d)| d.is_dir()).cloned().collect();
    if targets.is_empty() {
        targets.extend(dirs.into_iter().take(1));
    }
    let mut failed = false;
    for (name, dir) in targets {
        let path = dir.join("NativeMessagingHosts").join(format!("{HOST_NAME}.json"));
        let res = std::fs::create_dir_all(path.parent().expect("a parent")).and_then(|()| std::fs::write(&path, &manifest));
        match res {
            Ok(()) => println!("{name}: wrote {}", path.display()),
            Err(e) => {
                failed = true;
                eprintln!("{name}: could not write {}: {e}", path.display());
            }
        }
    }
    println!("The host runs {}. Load the extension from the chrome/ folder (chrome://extensions, Developer mode, Load unpacked).", binary.display());
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// A hub with a scripted extension behind it, for tests: `answer` gets each method and params.
#[cfg(all(test, unix))]
pub async fn fake_hub<F>(name: &str, answer: F) -> (PathBuf, tokio::task::JoinHandle<()>)
where
    F: Fn(&str, &Value) -> Result<Value, String> + Send + 'static,
{
    let sock = PathBuf::from(format!("/tmp/tx-{}-{name}.sock", std::process::id()));
    let (hub_out, mut ext_in) = tokio::io::duplex(1 << 20);
    let (mut ext_out, hub_in) = tokio::io::duplex(1 << 20);
    let path = sock.clone();
    let task = tokio::spawn(async move {
        let ext = async move {
            while let Ok(Some(msg)) = read_frame(&mut ext_in).await {
                let Some(id) = msg.get("id").cloned() else { continue };
                let reply = match answer(msg["method"].as_str().unwrap_or_default(), &msg["params"]) {
                    Ok(v) => json!({ "id": id, "result": v }),
                    Err(e) => json!({ "id": id, "error": { "message": e } }),
                };
                if write_frame(&mut ext_out, &reply).await.is_err() {
                    break;
                }
            }
        };
        tokio::join!(ext, async { hub(hub_in, hub_out, &path).await.ok() });
    });
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (sock, task)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn temp_socket(name: &str) -> PathBuf {
        // Unix socket paths are short (104 bytes on macOS), so not under a deep temp folder.
        PathBuf::from(format!("/tmp/tx-{}-{name}.sock", std::process::id()))
    }

    /// `chrome/status`: connected with the extension's answer to ping, or not connected.
    #[tokio::test]
    async fn status_says_whether_the_extension_answers() {
        assert_eq!(status_of(None).await["connected"], false);
        let (sock, hub) = fake_hub("status", |method, _| if method == "ping" { Ok(json!({ "version": "0.1.0", "paused": true })) } else { Err("no".into()) }).await;
        let s = status_of(Conn::connect_to(&sock).await.ok()).await;
        assert_eq!((s["connected"].clone(), s["paused"].clone(), s["version"].clone()), (json!(true), json!(true), json!("0.1.0")));
        hub.abort();
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn frames_round_trip_in_native_byte_order() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &json!({ "x": 1 })).await.unwrap();
        drop(a);
        let mut raw = Vec::new();
        b.read_to_end(&mut raw).await.unwrap();
        assert_eq!(u32::from_ne_bytes(raw[..4].try_into().unwrap()) as usize, raw.len() - 4);
        let v = read_frame(&mut &raw[..]).await.unwrap().unwrap();
        assert_eq!(v, json!({ "x": 1 }));
        assert!(read_frame(&mut &raw[..0]).await.unwrap().is_none(), "a closed stream is the end");
    }

    #[tokio::test]
    async fn a_message_too_large_for_chrome_is_refused() {
        let (mut a, _b) = tokio::io::duplex(16);
        let big = json!({ "s": "x".repeat(MAX_TO_EXTENSION) });
        assert_eq!(write_frame(&mut a, &big).await.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    /// A fake extension: answers `echo` with its params and sends one event first.
    async fn fake_extension(mut from_hub: tokio::io::DuplexStream, mut to_hub: tokio::io::DuplexStream) {
        while let Ok(Some(msg)) = read_frame(&mut from_hub).await {
            let Some(id) = msg.get("id").cloned() else { continue };
            write_frame(&mut to_hub, &json!({ "event": "tick" })).await.unwrap();
            let reply = if msg["method"] == "echo" {
                json!({ "id": id, "result": msg["params"] })
            } else {
                json!({ "id": id, "error": { "message": "unknown method" } })
            };
            write_frame(&mut to_hub, &reply).await.unwrap();
        }
    }

    #[tokio::test]
    async fn the_hub_routes_each_reply_to_the_runtime_that_asked() {
        let sock = temp_socket("route");
        let (hub_out, ext_in) = tokio::io::duplex(1 << 16);
        let (ext_out, hub_in) = tokio::io::duplex(1 << 16);
        tokio::spawn(fake_extension(ext_in, ext_out));
        let path = sock.clone();
        let hub_task = tokio::spawn(async move { hub(hub_in, hub_out, &path).await });
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&sock).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600, "only this user may reach the hub");

        let t = Duration::from_secs(5);
        let mut a = Conn::connect_to(&sock).await.unwrap();
        let mut b = Conn::connect_to(&sock).await.unwrap();
        // Both runtimes use id 1; each still gets its own reply.
        let (ra, rb) = tokio::join!(a.call("echo", json!({ "who": "a" }), t), b.call("echo", json!({ "who": "b" }), t));
        assert_eq!(ra.unwrap(), json!({ "who": "a" }));
        assert_eq!(rb.unwrap(), json!({ "who": "b" }));
        assert_eq!(a.call("nope", json!({}), t).await.unwrap_err(), "nope: unknown method");
        hub_task.abort();
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn chrome_starts_the_host_with_the_extension_origin() {
        assert!(is_host_launch(Some("chrome-extension://ifhmkecelafiikilbgjleeopcodpnbfe/")));
        assert!(!is_host_launch(Some("chat")) && !is_host_launch(None));
    }

    #[test]
    fn the_host_manifest_allows_only_the_given_extensions() {
        let m = host_manifest(Path::new("/opt/task-x-runtime"), &[EXTENSION_ID.to_owned()]);
        assert_eq!(m["name"], HOST_NAME);
        assert_eq!(m["type"], "stdio");
        assert_eq!(m["path"], "/opt/task-x-runtime");
        assert_eq!(m["allowed_origins"], json!([format!("chrome-extension://{EXTENSION_ID}/")]));
        assert!(host_dirs(Path::new("/h")).iter().all(|(_, d)| d.starts_with("/h")));
    }
}
