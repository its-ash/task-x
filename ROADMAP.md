# Roadmap

This file records the state of the work between sessions. Mark an item done only after a test or a manual check proves it.

**Last verified:**
- Runtime: `cargo test` passes 158 tests (150 unit, 8 protocol); `cargo clippy --all-targets` reports no warnings. The unit tests include a Team-mode run (plan → research and implementation subagents → merge into the edit log), best-of-N with two candidates, plan validation, cost estimates, checkpoint reload, a 70 KB skill file, forced summaries at the iteration and tool-call limits, destructive-tool opt-in, error classes, retry backoff, a fallback that rebuilds the request for the new model's window, a blocking hook that runs before the approval prompt, `web_fetch` refusing local and private targets, approval previews for every editing tool, streamed tool calls that share one index or repeat their arguments, subagent roles written in any capitalisation or as an alias, hidden folders pruned from the index while hidden files stay, `edit_files` accepting the batch shapes models send and a loose top level, `copy_file`/`move_file` bringing a file in from outside the workspace, a worktree the agent created being discarded without an opt-in, policy denials not counting as being stuck, a command whose pipe a child holds open no longer hanging, a slow command handed to the background while a quick one returns its output, a resumed task keeping its tool results, and interrupted (`running`) records marked failed at start.
- Extension: `npm test` passes 31 tests, including end-to-end runs through the built runtime with mock providers, a checkpoint reload across a runtime restart, and the review-findings parser.
- Manual checks:
  - `provider/test` against a local Ollama server
  - skill and instruction discovery on this machine (87 skills from `~/.copilot/skills`)
  - chat UI rendered in Chrome at 300 px and 500 px width

**Not verified yet:**
- Real Anthropic and OpenAI streaming (no keys used)
- The extension inside a VS Code Extension Development Host
- CI on Windows and Linux (the workflow is written but has not run)
- The timeline UI, the Anthropic sign-in flow, and the Logs panel inside VS Code
- Inside VS Code: the project graph panel, the index status, the @ file menu and context chips, syntax highlighting, notifications, subagent lanes, the Team mode tab, the Compare panel (best of N), background tasks and the status bar count, and review findings in the Problems panel
- Team mode and best of N against real models (only mock providers so far)

| Phase | Status | Where |
|---|---|---|
| 1 Foundation, providers, JSON-RPC | ✅ | `runtime/src/{rpc,server}.rs`, `runtime/src/provider/`, `extension/src/{rpc,runtime,providerDialog}.ts` |
| 2 Project discovery and context | ✅ | `runtime/src/context/`, `docs/context.md` |
| 3 Prompt enrichment | ✅ | `runtime/src/agent/prompt.rs` |
| 4 Tool registry (37 tools) | ✅ | `runtime/src/tools/` |
| 5 Agent loop | ✅ | `runtime/src/agent/{mod,store}.rs` |
| 6 VS Code experience | ✅ core | `extension/src/{chatView,editorCommands,workspace}.ts`, `extension/media/` |
| 7 Permissions, sandbox, recovery | ✅ | `agent/policy.rs`, `tools/exec.rs` (sandbox), `tools/mod.rs` (undo, keep/revert), `docs/security.md` |
| 8 Skills, hooks, MCP | ✅ | `agent/{skills,hooks}.rs`, `runtime/src/mcp.rs`, `docs/extensibility.md` |
| 9 Worktrees and subagents | ✅ | `agent/team.rs`, `run_subagent` / `worktree_apply` / `worktree_discard` |
| 10 Context and model routing | ✅ | `agent/compact.rs`, routes, fallbacks, and metrics in `agent/mod.rs` |
| 11 Review and QA | ✅ core | **Review Changes** command, validation runs, and skipped checks in each summary |
| 12 Testing and reliability | 🟡 | unit, protocol, end-to-end, and performance tests; `.github/workflows/ci.yml` |
| 13 Packaging and docs | 🟡 | `npm --prefix extension run package`, `make build`, `docs/` |
| 14 Team mode and parallel work | ✅ runtime, 🟡 UI | `run_plan` scheduler in `agent/mod.rs`, `team::create_with_base` and `team::merge`, `agent/bestOf`, `agent/cancelSubagent`, `worktrees/apply`; lanes in `media/chat.js`, `extension/src/{background,compareView}.ts` |
| 15 Cost, checkpoints, findings | ✅ | `agent/cost.rs`, `EditLog::open`/`save` in `tools/mod.rs`, `extension/src/findings.ts` |
| 16 Project graph and indexing status | ✅ runtime, 🟡 UI | `context/graph.rs`, `workspace/progress`, `graph/project`, `graph/file`, `get_dependencies`; `extension/src/graphView.ts`, `media/graph.js` |
| 18 Context compression | ✅ runtime, 🟡 UI | `agent/compact.rs` (prune, fold, drop excerpts), per-model windows in `provider/*.rs`, Ollama `num_ctx`, `context` event and meter |
| 19 Claude Code uses the Rust tools | ✅ runtime, 🟡 manual check with a real Claude run | `mcp_server.rs` (`--mcp-server`), `claude_cli::args` |
| 17 Input context | ✅ runtime, 🟡 UI | `@path` mentions (`EditorContext.mentions`), file and selection chips, syntax highlighting (highlight.js), notifications (`task-x.notifications`) |
| 20 Safety and observability | 🟡 | `agent/policy.rs` (destructive tools need an `allow` entry), `tools/mod.rs` (`ErrorClass`, `TOOL_ATTEMPTS`, `retry_backoff`), `agent/mod.rs` (`synthesize` at the iteration and tool-call limits, retries for read-only calls, `info!` events for model and tool calls), `main.rs` (`TASK_X_LOG`) |

## What each phase delivers

- **Team**:
  - Team mode: the lead calls `run_plan` with a dependency graph; ready steps run as parallel subagents (`task-x.team.maxParallel`), each writer in its own worktree that starts from the work of its dependencies
  - finished work merges in dependency order through the edit log (pending review, undoable); a conflicting step keeps its worktree
  - a model per role (`task-x.team.roleProviders`)
  - one lane per subagent in the chat: role, model, live steps, files and lines, cost, and Stop, Apply, Discard, and Open folder
  - **Best of N**: 2 to 4 models do the same task in separate worktrees; the Compare panel shows them side by side, and Keep applies one and discards the rest
  - **Background tasks**: run a task outside the chat; the status bar counts running work, and a notification opens the result
- **Chat**:
  - Ask, Plan, Agent, and Team modes as filled tabs, with model and approval pickers beside the input
  - streaming Markdown
  - tool activity on a timeline (one node per step: running, done, failed, file edit)
  - inline approval cards
  - a changed-files panel above the input (diff, Keep, and Undo per file or for all)
  - subagent and model-switch notes
  - task history
  - **task-x Logs** panel beside the terminal: extension, runtime, and agent logs with level, source, and text filters
- **Providers**:
  - Anthropic, OpenAI, OpenAI-compatible (key required), and Ollama
  - **Sign In with Anthropic**: `claude auth login` in a terminal; chat then runs through Claude Code with that account
  - the **Add LLM Provider** dialog
  - routing by task kind (`task-x.routing`)
  - opt-in fallback (`task-x.fallbackProviders`)
- **Context**:
  - an index that obeys `.gitignore` and always excludes secrets
  - Tree-sitter symbols
  - project detection
  - Copilot and Claude instruction files, including `.github/instructions/*.instructions.md` with `applyTo`
  - retrieval within a token budget
  - compaction for long tasks
- **Safety**:
  - the permission policy
  - workspace-only paths
  - stale-edit detection
  - atomic writes with rollback
  - undo that keeps your concurrent edits
  - the OS sandbox for commands (macOS `sandbox-exec`; Linux `bwrap`)
  - hooks that can block but not grant
- **Extensibility**:
  - skills from `.github/skills`, `~/.copilot/skills`, `.claude/skills`, `~/.claude/skills`, `.task-x/skills`, and `.github/prompts/*.prompt.md`
  - `/name` invocation, with a `/` menu in the chat input that lists skills as you type
  - skills for the Claude Code engine: `skills/resolve` adds the task-x skills that apply (Claude loads `.claude/skills` itself)
  - an on-demand skill index with `read_skill`
  - hooks
  - MCP servers over stdio

## Known gaps (next work)

0. Fixed after a review (verified by the tests above): the fallback model now gets a request rebuilt for its own window; `web_fetch` refuses local, private, and metadata targets (including through redirects); a blocking hook runs before the approval prompt; runs that share a task id wait for each other instead of overwriting the record; records left `running` by a killed runtime are marked failed at start; the sandbox is probed and skipped where it cannot be applied, so commands no longer fail with exit 71 in containers and nested sandboxes.
1. Run the extension in a real Extension Development Host. Add an `@vscode/test-electron` smoke test.
2. Platform-specific VSIX builds (`vsce package --target …`) with a runtime binary for each target. Today `make build` bundles only the host's binary.
3. Inline diff decorations in the editor. Diffs now open in VS Code's diff editor from the changed-files panel.
4. A Windows sandbox (commands run without one there).
5. Prices for providers other than Anthropic: they need `task-x.pricing` rules; until then their tokens show as unpriced.
6. Binary files in a worktree merge are applied by git but are not in the edit log, so undo does not restore them.
7. Requested agent features not yet built, in build order (each needs its own design and tests). Already in place: the token and call budgets with a forced final summary (`max_total_tokens`, `synthesize`), error classes (`errors.rs`, `tools::ErrorClass`), edit-log checkpoints, and batched tool execution (`join_all` in `run_calls`).
   - First: incremental re-indexing with a debounced watcher, and a persisted graph with a version check; a tool execution hook layer that unifies approval, budget, logging, and provenance; the blast-radius gate.
   - Second: query classification; graph tools as agent tools; graph-driven context loading; ranked repo map.
   - Third: cross-session memory (SQLite or JSONL is a decision still open); BM25 fallback; retrieval provenance log; typed tool results.
   - Fourth: self-evolving skills; scheduled jobs; self-evaluation loop; edge-weighted impact scoring; schema validation and prompt versioning.
   - Last: parallel tool calls within a turn beyond the current batching; real-model tests for Team mode and the Logs panel.
   - Query classification (structural, semantic, hybrid with reciprocal rank fusion) and graph tools (`call_graph`, `module_summary`, `imports_of`, `symbol_definition`). `find_definition`, `find_references`, and `get_dependencies` already exist.
   - Token-budgeted PageRank repository map at session start; graph-driven context loading; a blast-radius gate on edits.
   - Cross-session memory (SQLite or JSONL, recalled into the system prompt); self-evolving skill patches; agent self-evaluation.
   - Typed tool results (`Text`, `Json`, `Image`, `FileRef`, `Error`) and schema validation of tool inputs and outputs.
   - BM25 fallback when embeddings are unavailable; edge-weighted impact scoring; retrieval provenance view.
   - Scheduled jobs, a watch-mode daemon, and multi-channel output (webhook, file).
   - Checkpoint-resumable runs and tool-level cost breakdowns; prompt and tool versioning.
   - Incremental re-indexing: `Index::update(paths)` re-parses changed paths, but there is no mtime or content-hash scan and no debounced watcher. The graph is rebuilt in memory, so there is no persisted graph whose `GRAPH_ENGINE_VERSION` needs checking.
