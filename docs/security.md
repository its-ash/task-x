# Permissions and security

## Chat modes

| Mode | What the agent can do |
|---|---|
| **Ask** | Read and search. No edits, commands, or MCP tools. |
| **Plan** | Read and search, then write a plan. No edits. |
| **Agent** | Edit files and run commands in the workspace without asking. Commands always run in the sandbox. |

## Approvals (Agent mode)

Agent mode always edits; there is no read-only switch. The runtime gets the `autonomous` policy:

| Edits (including `worktree_apply` and `worktree_discard`) | Commands | Network, git writes, MCP tools |
|---|---|---|
| allowed | allowed, sandboxed | ask |

- `copy_file` and `move_file` are edits when the source is inside the workspace. A source **outside** it (an absolute path, or a path that climbs out with `..`) is `external`: it asks in every mode that can write, and a read-only mode refuses it. The destination is always inside the workspace.
- Tools are classed by risk. Reads run without asking. **Destructive** tools (file deletes and installs) are refused unless the tool is named in `task-x.agent.allowTools`. A session approval does not count as that opt-in; adding the tool to the list does.
- A refused call reaches the model as `[BLOCKED]` with the reason: a tool the mode does not offer, or one blocked by the policy. Blocked rounds never count towards "the model is stuck", because nothing it sends can change them; a denial the user chose still does.
- `task-x.agent.allowTools` lists tools that never ask. `task-x.agent.denyTools` lists tools that are always refused.
- The Claude Code engine runs with `acceptEdits` and Bash allowed. Its Bash runs in Claude Code's own sandbox (`sandbox.enabled` in `--settings`), and its other permission prompts go to the task-x approval card. The sandbox network allows package registries and toolchain hosts for the common languages (crates.io, npm, PyPI, Go proxy, Maven, Gradle, pub.dev, RubyGems, Packagist, NuGet, Hex, and others), plus GitHub and GitLab for git dependencies (`sandbox.network.allowedDomains`), so dependency downloads work. Other hosts stay blocked.
- `task-x.agent.denyTools` lists tools that are always refused.
- **Allow for session** stops the prompts for one tool until the runtime restarts.
- **Stop** cancels the current request immediately, including running commands.

Only this configuration and your approvals control permissions. Instructions, skills, repository files, and tool output reach the model as data and cannot change permissions. The runtime enforces each decision before a tool runs.

## File safety

- Tools accept only paths inside the workspace. They reject `..`, absolute paths outside the root, and symlinks that point outside it. They cannot touch `.git/`. Secret-looking files (`.env*`, keys, credentials) are readable and searchable like any file; their edit diffs show no content. See [context.md](context.md).
- Every edit checks that the text matches exactly and appears the expected number of times. An edit fails when the file changed on disk after the agent last read it.
- Writes are atomic (a temp file, then a rename). A multi-file patch changes all files or none.
- The changed-files panel above the chat input has **Keep** and **Undo**. Undo restores a file only if nobody changed it after the agent's edit, so your concurrent work is kept.

## Commands

- Commands come from the project's manifests (format, lint, test, build). They are argument arrays and never run through a shell.
- Each command has a fixed working folder, a timeout, and an output limit. The whole process group is killed on timeout or cancel.
- **Long commands keep running beside the agent.** A `run_command` still running after 30 seconds is handed to the background: the model gets a note instead of blocking, the agent keeps working, and the output arrives as a message when the command ends. The task never finishes while one is still running. Pass `background: true` to skip the wait entirely. Builds, tests, installs, and fetches (`cargo build`, `npm install`, `pytest`, `go test`, `gradle`, `flutter build`, and the like) start in the background at once unless the call passes `background: false`. The Claude Code engine does the same: its Bash hook sets `run_in_background` on those commands, and in ask mode the call still needs approval.
- A command's output is read until the pipe closes, or for at most two seconds after the command itself ends. A descendant that inherited the pipe (a detached daemon, a `head` in a pipeline that outlived its parent) therefore cannot leave a call stuck on "Running…".
- **Sandbox** (always on):
  - macOS: `sandbox-exec`. Linux: `bwrap`, when installed.
  - Commands can write only to the workspace, temp folders, and build caches (`~/.cargo`, `~/.npm`, `~/.cache`, …).
  - Reads and network stay allowed, so builds and dependency downloads work.
  - The sandbox is probed once, at the first command. Where it exists but cannot be applied (a nested sandbox, a container, some CI images), commands run without it instead of failing, and the result says `sandboxed: false`. On Windows, and on Linux without `bwrap`, that is always the case.

## Web access

- `web_fetch` reads public pages only. It refuses loopback, private, link-local, and other non-public addresses, cloud metadata endpoints (`169.254.169.254`, `metadata.google.internal`), and names such as `localhost` or `*.internal` — including when a redirect points at one. Pages are untrusted input, so a poisoned page cannot use the agent to reach a service on your machine.
- `browser` drives a real Chrome or Edge, or VS Code itself, over the DevTools Protocol (CDP). It is the one tool that must reach loopback, so its endpoint has its own rule: only `http` on `127.0.0.1`, `::1`, or `localhost` is accepted, and a non-local endpoint is refused with a message that says how to start the browser. The endpoint is `TASK_X_CDP_ENDPOINT`, default `http://127.0.0.1:9222`.
- Inside VS Code, `browser` drives VS Code's **integrated browser** with no debug port and no restart. The extension serves a loopback bridge on `127.0.0.1` (random port) that calls only VS Code's own browser tools (`open_browser_page`, `navigate_page`, `read_page`, `click_element`, `type_in_page`, `screenshot_page`, `run_playwright_code`, and the other browser tools). Each request needs a random 256-bit bearer token. The runtime gets the address and token in `TASK_X_VSCODE_BROWSER` and `TASK_X_VSCODE_BROWSER_TOKEN`, and the Claude Code engine's MCP server gets them through its `--mcp-config` environment, the same way as the approval token. VS Code's own rules still apply: its network filter, and its prompts before a page is shared. When `TASK_X_CDP_ENDPOINT` (or `task-x.cdpEndpoint`) is set, that endpoint is used instead.
- To drive a VS Code **Simple Browser** view over CDP, quit VS Code and start it with `code --remote-debugging-port=9222`. The page in the view is an `iframe` target and is driven like a tab; `open` and `close` are not available there, and `screenshot` can be refused. VS Code's own UI (`vscode-file:` and `vscode-webview:` targets) is never offered to the agent. While VS Code runs with this port, every local process can control the VS Code window.
- `browser` has the `network` permission class: it needs approval unless `task-x.agent.approveAll` is on or the tool is in `task-x.agent.allowTools`. `browser` can run arbitrary JavaScript in the page (`eval`), so treat it as running untrusted input. Pages opened with it are not restricted to public hosts — it is your own browser, so a page it opens may reach your intranet, exactly as it would if you opened it.

## Secrets and privacy

- API keys live in VS Code SecretStorage (the OS keychain). They are never written to settings, logs, or task files.
- The runtime refuses to send a key over plain `http` to a non-loopback host.
- The runtime starts with an allowlist of environment variables.
- Model fallback (`task-x.fallbackProviders`) is off by default. It uses only the providers you list, and the chat reports every switch.

## Subagents and worktrees

- Subagents that write (implementation, testing, documentation) each get a git worktree outside the workspace (`../.anvil-worktrees/<repo>/<name>`).
- Their changes come back as a patch. The lead agent applies a patch with `worktree_apply`. It is an edit for approval purposes, and it refuses when the patch conflicts with the working tree.
- `worktree_discard` is an edit too: it removes a worktree **this task created**, outside your working tree, so it needs no opt-in. Both tools refuse a name the task did not create.
- A worktree with changes that were never applied is not removed without an explicit discard.
