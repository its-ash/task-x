# Roadmap

This file records the state of the work between sessions. Mark an item done only after a test or a manual check proves it.

**Last verified:**
- Runtime: `cargo test` passes 285 tests (274 unit, 11 protocol; 2026-11-12, after wake-ups, the timeline, and HTML export); `cargo clippy --all-targets` reports no warnings. New in this round: hook events and JSON answers (stop sends the agent back, prompt context, a trusted hook that approves and rewrites input, Claude-format plugin hooks with exit code 2), background and nested subagents, checkpoints with rewind, fork, compact, and cost (unit and protocol tests), built-in `/init`, the marketplace (add, install, update, uninstall from folder sources), the change scan, the saved graph and PageRank, the repo map in the first request, `recall` across tasks, OAuth sign-in with PKCE and refresh against a local OAuth and MCP server, and the CLI (`exec` with `--json` against a mock model, exit codes). The unit tests include custom agents (Claude Code agent files, `run_subagent { agent }`, a plan step with an agent, `@name` for a whole task, model aliases), commands (`$ARGUMENTS`, `$1`, folder namespaces, `allowed-tools`, kept out of the skill index), plugins (Claude Code installs with `enabledPlugins`, `.task-x/plugins`, `.task-x/plugins.json`), and a protocol test for `plugins/configure`, `plugins/list`, `agents/list`, and plugin commands in `skills/resolve`. They also include prompt caching (breakpoints, and cache reads priced at a tenth), shortened command output with `read_log`, repeated reads answered with a note, the 300-line read default with an outline, `multi_search` and multi-name `read_symbol`, the lazy tool list (`find_tool`, and a deferred tool loaded by name), `summarize_file` through the helper model, project memory in the prompt, a Team-mode run (plan → research and implementation subagents → merge into the edit log), best-of-N with two candidates, plan validation, cost estimates, checkpoint reload, a 70 KB skill file, forced summaries at the iteration and tool-call limits, destructive-tool opt-in, error classes, retry backoff, a fallback that rebuilds the request for the new model's window, a blocking hook that runs before the approval prompt, `web_fetch` refusing local and private targets, approval previews for every editing tool, streamed tool calls that share one index or repeat their arguments, subagent roles written in any capitalisation or as an alias, hidden folders pruned from the index while hidden files stay, `edit_files` accepting the batch shapes models send and a loose top level, `copy_file`/`move_file` bringing a file in from outside the workspace, a worktree the agent created being discarded without an opt-in, policy denials not counting as being stuck, a command whose pipe a child holds open no longer hanging, a slow command handed to the background while a quick one returns its output, a resumed task keeping its tool results, and interrupted (`running`) records marked failed at start.
- Extension: `npm test` passes 43 tests, including end-to-end runs through the built runtime with mock providers, a checkpoint reload across a runtime restart, and the review-findings parser.
- Manual checks:
  - `provider/test` against a local Ollama server
  - skill and instruction discovery on this machine (87 skills from `~/.copilot/skills`)
  - chat UI rendered in Chrome at 300 px and 500 px width

**Not verified yet:**
- The MCP HTTP transport test, and the tests that open a local port, now run in this session's sandbox and pass (`mcp::tests::talks_to_a_streamable_http_server`, `serves_the_tool_registry_over_mcp`, the checkpoint/restart tests); a sandbox that blocks local ports still skips them.
- Prompt caching against the real Anthropic API: the breakpoints and the cache-token accounting are covered by unit tests, but no request was made with a key, so the cache actually being read is unproven. OpenAI's automatic caching likewise.
- Claude Code with `sandbox.allowUnsandboxedCommands: false` in a real run
- Real Anthropic and OpenAI streaming (no keys used)
- The extension inside a VS Code Extension Development Host
- CI on Windows and Linux (the workflow is written but has not run)
- The timeline UI, the Anthropic sign-in flow, and the Logs panel inside VS Code
- Inside VS Code: the project graph panel, the index status, the @ file menu and context chips, syntax highlighting, notifications, subagent lanes, the Team mode tab, the Compare panel (best of N), background tasks and the status bar count, and review findings in the Problems panel
- Team mode and best of N against real models (only mock providers so far)
- The `browser` tool against a live Chrome: only its endpoint check, argument validation, and JavaScript escaping are covered by tests; click, type, and navigation have not run against a real page
- The token savers against real models: the lazy tool list, `summarize_file` with a real helper model, and the shortened output are covered with mock providers; whether a model actually calls `find_tool` instead of inventing a tool is not
- In the chat UI: the running-tab timer and Stop, the token chip on tool rows, the tables in replies and in your own messages, the Refine and Helper model rows in the settings dialog, and the backtick wrapping in the input

| Phase | Status | Where |
|---|---|---|
| 1 Foundation, providers, JSON-RPC | ✅ | `runtime/src/{rpc,server}.rs`, `runtime/src/provider/`, `extension/src/{rpc,runtime,providerDialog}.ts` |
| 2 Project discovery and context | ✅ | `runtime/src/context/`, `docs/context.md` |
| 3 Prompt enrichment | ✅ | `runtime/src/agent/prompt.rs` |
| 4 Tool registry (47 tools) | ✅ | `runtime/src/tools/` |
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
| 21 MCP from Claude and Copilot, HTTP transports | ✅ runtime and import (unit tests), 🟡 manual check with Figma and Chrome DevTools | `runtime/src/mcp.rs` (stdio, streamable HTTP, HTTP+SSE), `extension/src/{mcpImport,mcpSources}.ts`, `task-x.mcp.import`, `claude/run` `mcpServers` |
| 22 Images in chat | ✅ runtime (unit tests), 🟡 UI and real models | `ContentPart::Image` and `image_parts` in `provider/types.rs`, all three providers, `agent/run` and `claude/run` `images`; paste, drop, and pick in `media/chat.js` |
| 24 Provider dialog and model picker | ✅ runtime and list logic (unit tests), 🟡 not checked inside VS Code | `provider/models` (`ModelInfo` in `provider/types.rs`), `extension/src/{models,providerDialog}.ts`, picker in `media/chat.js` |
| 23 Release | 🟡 not run yet | `make deploy` (publishes `task-x.vsix` on the GitHub release), `make add` (installs it) |
| 20 Safety and observability | 🟡 | `agent/policy.rs` (destructive tools need an `allow` entry), `tools/mod.rs` (`ErrorClass`, `TOOL_ATTEMPTS`, `retry_backoff`), `agent/mod.rs` (`synthesize` at the iteration and tool-call limits, retries for read-only calls, `info!` events for model and tool calls), `main.rs` (`TASK_X_LOG`) |
| 25 Browser automation | ✅ runtime (unit tests), ✅ VS Code integrated browser (`open` checked live on VS Code 1.143), 🟡 not checked against a live Chrome or a VS Code Simple Browser over CDP | `tools/browser.rs` (CDP over a websocket), the `browser` tool: tabs, open, goto, click, type, press, screenshot, text, html, eval, wait, back, forward, close; `docs/security.md` |
| 27 Custom agents, commands, and plugins | ✅ runtime (unit and protocol tests), 🟡 UI not checked inside VS Code | `agent/agents.rs` (`.claude/agents`, `@name`, `run_subagent { agent }`, plan steps with `agent`), `agent/skills.rs` (`Kind::Command`, `$ARGUMENTS`, plugin namespaces), `agent/plugins.rs` (Claude Code installs, `.task-x/plugins`, `plugins/configure`); `agents/list`, `plugins/list`; `/` and `@` menus in `media/chat.js` |
| 28 Terminal CLI | ✅ runtime (unit tests), 🟡 not run against a real model | `runtime/src/cli.rs`: `task-x-runtime exec` (one task, exit code, `--json`) and `chat` (interactive, with `/cost`, `/compact`, `/rewind`, `/fork`) |
| 29 Hook events and JSON answers | ✅ runtime (unit, agent, and protocol tests) | `agent/hooks.rs` (`userPromptSubmit`, `stop`, `subagentStop`, `preCompact`, `notification`, Claude Code names, block, approve, `updatedInput`, `additionalContext`, plugin `hooks/hooks.json`) |
| 30 Plugin marketplace without Claude Code | ✅ runtime (unit tests with folder sources), 🟡 git clone of a real marketplace not run, UI not checked inside VS Code | `agent/marketplace.rs`, `plugins/install`, `plugins/marketplace*`; **Install Plugin…**, **Add Plugin Marketplace…** |
| 31 Session controls | ✅ runtime (unit and protocol tests), 🟡 UI not checked inside VS Code | `agent/session.rs` (checkpoints, rewind with undo, fork, compact, cost), built-in `/init`, `/review`, `/security-review`, `/create-skill`, `/create-agent`, background subagents, `limits.maxDepth` |
| 32 MCP OAuth sign-in | ✅ runtime (against a local OAuth and MCP server), 🟡 not run against a real provider, UI not checked inside VS Code | `runtime/src/oauth.rs` (discovery, registration, PKCE, loopback, refresh), `mcp/authorize`, `extension/src/mcpAuth.ts` (SecretStorage) |
| 33 Context engine | ✅ runtime (unit tests) | `Index::scan` and the watcher (`workspace/open { watch }`), the saved graph (`ProjectGraph::load_or_build`), `context/repomap.rs` (PageRank map, `repo_map`), `agent/recall.rs` (`recall`, `memory/search`) |
| 34 Parallel chat tabs | ✅ type check and extension tests, 🟡 not checked inside VS Code | `extension/src/chatView.ts`: one `Run` per chat tab (abort, approvals, questions, steering, cards, timer), `AsyncLocalStorage` scope per run, runtime requests routed by `taskId`, up to `MAX_PARALLEL_RUNS` (4) runs at once, a queue per tab, per-tab Stop in `media/chat.js` |
| 26 Fewer tokens per turn | ✅ runtime (unit tests), 🟡 not checked against real models | prompt caching (`provider/anthropic.rs`, `cached_tokens`, `agent/cost.rs`), `tools/logs.rs` (`read_log`), repeated reads and the 300-line read default (`agent/mod.rs`, `tools/fs.rs`), `multi_search` and multi-name `read_symbol` (`tools/{search,fs}.rs`), the lazy tool list (`task-x.agent.lazyTools`, `find_tool`), `summarize_file` with the helper model, `tools/memory.rs` (`remember`, `.task-x/memory.md`), the token chip in `media/chat.js`; `docs/protocol.md` *Token savers* |
| 35 Claude Code engine: fewer tokens | ✅ runtime (unit tests), type check and extension tests, 🟡 not measured against real Claude Code usage | `runtime/src/claude_cli.rs`: index excerpts cut to 2,500 tokens; a resumed session gets them only when the prompt names a file or symbol (`names_code`); `subagentModel` → `CLAUDE_CODE_SUBAGENT_MODEL` (`task-x.claudeCode.subagentModel`, default `sonnet`); `docs/protocol.md` *Claude Code engine* |
| 36 Vector graphics (logos, icons) | ✅ runtime (unit and agent tests with a mock provider), 🟡 not checked against real models or inside VS Code | `tools/vector.rs` (`render_svg`: resvg render to PNG, sent to the model as an image; report with `contentBox`, colours, warnings; no file or URL loads), built-in on-demand skill `vector-design` in `agent/skills.rs`; `export_png` (PNG sizes, edit log); built-in custom agent `designer` (`agent/agents.rs`); SVG preview with Open and Export PNG in `media/chat.js` and `chatView.ts` (`extension/src/vector.ts`, unit tests); image results are no longer cut by the 60,000-character limit |
| 37 Wake-ups and waiting | ✅ runtime (unit, agent, MCP server, and Claude Code channel tests with mock providers and a paused clock), 🟡 not run against a real model or a real Claude Code session | `tools/schedule.rs` (`sleep`, `wait_until`), `agent/wakeup.rs` (`schedule_wakeup`, `list_wakeups`, `cancel_wakeup`; status `sleeping`, sleep time off the deadline), `claude_cli.rs` (`op: "tool"` over the approval channel, stdin kept open while wake-ups wait), `cli.rs` (☾ magenta / ⏰ yellow lines); `docs/protocol.md` *Wake-ups and waiting*. Extension (restored 2026-11-14): `sleeping`/`awake`/`wakeupScheduled` show as chat notes (`chatView.ts`), type check only |
| 38 Task timeline and HTML export | ✅ runtime (unit and agent tests: snapshots, diff, restore and its undo, ignored and large files, the project's git not seeing it, pruning to the last 10 tasks, a full task exported), 🟡 not run on a large real repository or with Claude Code | `agent/timeline.rs` (shadow repository `.task-x/timeline.git`, `refs/timeline/<taskId>`), snapshots in `agent/mod.rs` and `claude_cli.rs`, `agent/export.rs` (self-contained HTML), `timeline/*` and `agent/export` in `server.rs`, `/timeline`, `/restore`, `/export` in `cli.rs`; `docs/protocol.md` *Timeline and export* |
| 39 Export a range of turns, themed page | ✅ runtime (unit tests: turns and ranges from checkpoints, stale and missing checkpoints, the page for a range, aligned and framed tables, the CSS theme switch, `/export` range parsing; 289 tests pass and clippy is clean, 2026-11-13), 🟡 the page not yet checked by eye in a browser | `agent/export.rs` (`turns`, `span`, `Range`), `agent/turns` and `from`/`to` on `agent/export` in `server.rs`, the start/end picker for `/export` in `cli.rs`; `docs/protocol.md` *Timeline and export* |
| 40 Tool approvals list | ✅ runtime (unit tests: decisions and reasons per tool and server, session grants, `*` patterns, set to allow/ask/deny with `coveredBy`; protocol test of `tools/approvals` and `tools/setApproval`; `<kbd>` in the export and the terminal; 295 tests pass and clippy is clean, 2026-11-13), 🟡 the extension's Tool Approvals screen uses it (row 46), not run against a real MCP server | `agent/approvals.rs`, `tool_matches` in `agent/policy.rs`, `tools/approvals` and `tools/setApproval` in `server.rs`, `/approvals`, `/allow`, `/ask`, `/deny` in `cli.rs`; `docs/protocol.md` |
| 41 MCP servers from Claude, VS Code, Copilot CLI, and Cursor | ✅ runtime (unit tests: every source file, precedence, workspace trust, variables, JSONC, `local` type, registry names shortened, skip notes; protocol test of `mcp/discover` and of filling in what a client's own import skipped; checked on this machine 2026-11-13: chrome-devtools from `~/.copilot/mcp-config.json` starts with 29 tools; whole-server allow/ask/deny clears per-tool exceptions), 🟡 not yet seen inside VS Code after a reload; the extension's MCP chip cannot turn off a server the runtime filled in | `mcp_import.rs`, `import`/`disabled` on `mcp/configure` and `mcp/discover` in `server.rs`, `local` in `mcp.rs`, import by default plus `/mcp` and `--no-mcp-import` in `cli.rs`; `docs/protocol.md` |
| 42 Security batch from the code review | ✅ runtime (tests: deleting commands found behind sudo/xargs/sh -c/chains and asked for in auto mode end to end, secrets out of retrieval, git without fsmonitor/hooks/textconv, sandbox keeps .git/hooks and .git/config read-only (macOS), task-x secrets out of child environments and MCP `${env:}`, YAML `tools:` lists and `tools: []` fail closed, workspace skills/agents/plugins only when trusted; 293 unit + 13 protocol tests pass, clippy clean, 2026-11-13), 🟡 bwrap rules not run on Linux; the terminal trust prompt not tried by hand | `tools/validate.rs` `command_deletes`, `policy.rs` `decide_command_delete`, `retrieve.rs`, `tools/git.rs` `SAFE_GIT`, `tools/exec.rs` (`read_only_paths`, `private_env`), `agents.rs`/`skills.rs` front matter, `plugins::enabled_dirs`, `--trust` in `cli.rs`; `docs/security.md` |
| 43 Proceed with recommendation | ✅ runtime (`recommended` on `ask_user` questions and Claude Code's "(Recommended)" labels, `tookRecommendation` in the result, Enter takes all in the terminal; unit test), 🟡 the VS Code button is in `media/chat.js` (row 46), not clicked through | `agent/mod.rs` (`Question.recommended`, `recommended_options`), `claude_cli.rs`, `cli.rs` `ask_terminal`; `docs/protocol.md` `agent/askUser` |
| 44 Tracking several agents | ✅ runtime (`agent/activity.rs` kept from task events: tree of lead and subagents with status, current tool, turn, tools, tokens; `agent/active`; terminal board on each subagent start and end, `/agents`; unit test), 🟡 VS Code: a Show Active Agents picker only (row 46), no live panel | `agent/activity.rs`, `agent/active` in `server.rs`, `agents_board` in `cli.rs`; `docs/protocol.md` |
| 45 Reliability batch from the code review | ✅ runtime (tests: per-model output limits, agent turns ask for them, a cut-off answer continued and joined, an unreadable tool call sent again, `Retry-After` and stream error classes, a crashed background job counted as ended and reported; 297 unit + 13 protocol tests pass, clippy clean, 2026-11-13), 🟡 Ctrl-C in the terminal client and a panicking agent loop or request handler not exercised by a test | `provider::output_tokens` (`anthropic.rs`, `openai.rs`), `ErrorKind::BadToolCall`, `retry_after_ms` and `from_stream_error` in `provider/types.rs`, `retry_wait`, cut-off handling, `BackgroundJob`, the loop in its own task, and budget counting subagents in `agent/mod.rs`, `catch_unwind` in `server.rs`, `watch_ctrl_c` in `cli.rs`; `docs/protocol.md` |
| 46 Extension restored: session screens | ✅ type check and extension tests (48 pass, 2026-11-14, with an end-to-end test of `agent/turns`, `agent/export`, `timeline/*` restore and undo, `tools/approvals`/`setApproval`, `agent/active` through the built runtime), 🟡 not clicked through in VS Code | `extension/` restored from before `16c6518`; `src/sessionTools.ts` (Export as HTML with a turn range, Timeline with diff and restore, Tool Approvals saving `agent.allowTools`/`denyTools`, Active Agents); `/export`, `/timeline`, `/restore`, `/approvals`, `/agents-active` in the chat; recommended options and **Proceed with recommendation** in question cards (`media/chat.js`); `interrupted` offers Continue; `make ext`, `ext-install`, `ext-run` |

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
7. Requested agent features not yet built, in build order (each needs its own design and tests). Already in place: the token and call budgets with a forced final summary (`max_total_tokens`, `synthesize`), error classes (`errors.rs`, `tools::ErrorClass`), edit-log checkpoints, batched tool execution (`join_all` in `run_calls`), and the token savers of phase 26 (prompt caching, `read_log`, repeated reads, the lazy tool list, `summarize_file`, and project memory).
   - First: incremental re-indexing with a debounced watcher, and a persisted graph with a version check; a tool execution hook layer that unifies approval, budget, logging, and provenance; the blast-radius gate.
   - Second: query classification; graph tools as agent tools; graph-driven context loading; ranked repo map.
   - Third: cross-session memory as a searchable store (SQLite or JSONL is a decision still open) — `remember` and `.task-x/memory.md` now carry short facts into every task, but there is no recall, search, or pruning of old notes; BM25 fallback; retrieval provenance log; typed tool results.
   - Fourth: self-evolving skills; scheduled jobs; self-evaluation loop; edge-weighted impact scoring; schema validation and prompt versioning.
   - Last: parallel tool calls within a turn beyond the current batching; real-model tests for Team mode and the Logs panel.
   - Query classification (structural, semantic, hybrid with reciprocal rank fusion) and graph tools (`call_graph`, `module_summary`, `imports_of`, `symbol_definition`). `find_definition`, `find_references`, and `get_dependencies` already exist.
   - Token-budgeted PageRank repository map at session start; graph-driven context loading; a blast-radius gate on edits.
   - Cross-session memory recall beyond the 6,000-character `.task-x/memory.md` block (`remember`); self-evolving skill patches; agent self-evaluation.
   - Typed tool results (`Text`, `Json`, `Image`, `FileRef`, `Error`) and schema validation of tool inputs and outputs.
   - BM25 fallback when embeddings are unavailable; edge-weighted impact scoring; retrieval provenance view.
   - Scheduled jobs, a watch-mode daemon, and multi-channel output (webhook, file).
   - Checkpoint-resumable runs and tool-level cost breakdowns; prompt and tool versioning.
   - Incremental re-indexing: `Index::update(paths)` re-parses changed paths, but there is no mtime or content-hash scan and no debounced watcher. The graph is rebuilt in memory, so there is no persisted graph whose `GRAPH_ENGINE_VERSION` needs checking.
