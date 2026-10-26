mod agent;
mod claude_cli;
mod context;
mod errors;
mod mcp;
mod mcp_server;
mod provider;
mod rpc;
mod server;
mod tools;

use futures_util::StreamExt;
use server::{Flow, Server};
use std::process::ExitCode;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, LinesCodec};

const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Logs go to stderr because stdout carries the JSON-RPC stream. `TASK_X_LOG` sets the level
/// (error, warn, info, debug, trace); the default is warn, so a normal run logs only problems.
fn init_logging() {
    let level = std::env::var("TASK_X_LOG").ok().and_then(|v| v.parse().ok()).unwrap_or(tracing::Level::WARN);
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_max_level(level).with_ansi(false).init();
}

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("task-x-runtime {} (protocol {})", env!("CARGO_PKG_VERSION"), rpc::PROTOCOL_VERSION);
        return ExitCode::SUCCESS;
    }
    // `--hook-shell-edit`: Claude Code PreToolUse hook that blocks file writes from the shell.
    if std::env::args().nth(1).as_deref() == Some("--hook-shell-edit") {
        let mut input = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
        return match claude_cli::shell_edit_hook(&input) {
            Ok(()) => ExitCode::SUCCESS,
            Err(reason) => {
                eprintln!("{reason}");
                ExitCode::from(2)
            }
        };
    }
    // `--mcp-server [--root DIR]`: serve the tools to another agent (Claude Code) over MCP.
    let argv: Vec<String> = std::env::args().collect();
    if argv.iter().any(|a| a == "--mcp-server") {
        let root = argv.iter().position(|a| a == "--root").and_then(|i| argv.get(i + 1)).map(std::path::PathBuf::from);
        let root = match root.map_or_else(std::env::current_dir, Ok) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[task-x-runtime] mcp server: {e}");
                return ExitCode::FAILURE;
            }
        };
        let approval = std::env::var(claude_cli::APPROVAL_ENV).ok().and_then(|v| v.split_once(' ').map(|(a, t)| (a.to_owned(), t.to_owned())));
        return mcp_server::serve(root, !argv.iter().any(|a| a == "--no-tools"), approval).await;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(mut line) = rx.recv().await {
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let server = match Server::new(tx) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[task-x-runtime] startup failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut lines = FramedRead::new(tokio::io::stdin(), LinesCodec::new_with_max_length(MAX_MESSAGE_BYTES));
    while let Some(line) = lines.next().await {
        match line {
            Ok(l) if server.handle_line(&l) == Flow::Exit => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("[task-x-runtime] input error: {e}");
                break;
            }
        }
    }

    let _ = tokio::time::timeout(DRAIN_TIMEOUT, server.drain()).await;
    drop(server);
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, writer).await;
    ExitCode::SUCCESS
}
