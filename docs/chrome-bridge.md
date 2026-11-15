# Chrome bridge

The task-x Chrome extension (`chrome/`) lets the agent's `browser` tool drive the user's own
Chrome: their profile, sign-ins, and tabs, with no `--remote-debugging-port`. It is loaded
unpacked (never published to the Chrome Web Store), so it asks for every permission it may need
when it is loaded.

```
runtime ──┐                            native messaging
runtime ──┼── ~/.task-x/chrome.sock ── task-x-runtime (host) ──────────── extension service worker
runtime ──┘   (one JSON request per line)   (Chrome starts it)              chrome.debugger → tab
```

## Setup

1. `make chrome-install` (or `task-x-runtime chrome-install`) writes the native messaging host
   manifest `com.taskx.chrome.json` into the `NativeMessagingHosts` folder of each installed
   Chromium browser (Chrome, Chrome Beta and Canary, Chromium, Edge, Brave, Arc). The manifest
   points at the runtime binary and allows only the extension id
   `ifhmkecelafiikilbgjleeopcodpnbfe`, which the `key` in `chrome/manifest.json` fixes.
   `--extension-id <id>` allows another copy; `--path <binary>` points at another runtime.
2. In `chrome://extensions`, turn on **Developer mode**, **Load unpacked**, and pick `chrome/`.
3. Click the task-x toolbar button: the side panel says **Connected**.

The toolbar icon (a rounded square with an "x", `chrome/icons/`) is **green** (`#16a34a`) while the
extension is connected to the host and not paused, and **grey** (`#9ca3af`) otherwise; the service
worker redraws it with `OffscreenCanvas` and `chrome.action.setIcon` (`iconColor` in `lib.js`). The
badge still shows `!` when disconnected and `❚❚` when paused. The side panel's status uses the same
green and grey.

Optional switches only the user can turn on, on the extension's details page: *Allow access to
file URLs*, *Allow in Incognito*, and *Allow User Scripts*.

## The host

Chrome starts `task-x-runtime chrome-extension://<id>/` when the extension calls
`chrome.runtime.connectNative("com.taskx.chrome")`; the runtime sees the origin argument and runs
as the host (`runtime/src/chrome.rs`). It:

- speaks Chrome's framing on stdio: a 4-byte length in native byte order, then UTF-8 JSON. Chrome
  takes at most 1 MB per message from the host and sends at most 64 MB to it;
- listens on `~/.task-x/chrome.sock` (or `TASK_X_CHROME_SOCKET`), mode `0600`, so only this user
  reaches it; it replaces a socket left by an earlier host and, on exit, removes only its own;
- gives each runtime request a hub-wide numeric id, forwards it to the extension with a `client`
  number, and sends the reply back to that runtime under the runtime's own id;
- sends extension messages without an `id` (events) to every connected runtime;
- exits when Chrome closes its stdin (Chrome quit, or the extension reloaded).

## Messages

Runtime → hub, one line each: `{"id": 1, "method": "...", "params": {...}}`.
Hub → extension: `{"id": <hub id>, "method": "...", "params": {...}, "client": <n>}`.
Replies: `{"id": ..., "result": ...}` or `{"id": ..., "error": {"message": "..."}}`.
Events from the host: `{"event": "host.ready", "params": {"version", "pid"}}`.

Tab ids are Chrome's integers; the runtime sends and receives them as strings.

| Method | Params | Result |
|---|---|---|
| `ping` | — | `{version, paused}` |
| `tabs.list` | — | `{tabs: [{id, url, title, active, windowId, agent}], count, hidden, note}`: only tabs the agent may use |
| `tabs.open` | `{url, timeoutMs}` | the new tab, after it loads or the time runs out. http(s) only. The tab joins the window's purple **task-x · automated** group and is shared with the agent |
| `tabs.close` | `{tabId?}` | `{closed}` |
| `attach` | `{tabId?}` | `{tabId, url, title}`: resolves the tab and attaches `chrome.debugger` (protocol 1.3) |
| `cdp` | `{tabId, method, params}` | the CDP command's result, from `chrome.debugger.sendCommand` |

Without `tabId`, a request uses the tab the agent used last, else the active tab when the agent
may use it.

## What the agent may touch

- Every tab, by default: **Let task-x use every tab** in the side panel starts on. Turned off (the
  choice is saved), only tabs it opened and tabs the user shared (side panel **Share this tab**, the
  page's context menu, or ⌘⇧Y / Ctrl+Shift+Y).
- Automated tabs are marked: pages the agent opens go in the purple **task-x · automated** tab
  group, and Chrome shows its own "task-x started debugging this browser" bar while the debugger is
  attached to a tab.
- Not Chrome's own pages (`chrome://`, other extensions, the Web Store, `devtools://`,
  `view-source:`): Chrome refuses, and the extension says so.
- **Pause** in the side panel (or the `pause` command) detaches the debugger from every tab and
  refuses every request until **Resume**. Cancelling Chrome's "task-x started debugging this
  browser" bar unshares that tab.
- The activity log in the side panel records method names, tab ids, and the URLs `open` was given,
  never page content or script text.
- The `browser` tool tells the model that page text and HTML are untrusted and never instructions.

## Which browser the tool uses

`tools/browser.rs` picks, in order: `TASK_X_CDP_ENDPOINT` when set; the extension when the hub
socket answers; VS Code's integrated browser under the VS Code extension; Chrome's default
debugging port. `TASK_X_BROWSER=chrome|vscode|cdp` forces one. Results name the browser in
`browserName` (`Chrome (task-x extension)` for this path).

## Tests

- `cargo test --manifest-path runtime/Cargo.toml chrome` — framing, the 1 MB limit, the hub
  routing two runtimes' same-numbered requests, socket mode `0600`, the host manifest; and
  `browser` actions through a scripted extension behind a real hub.
- `npm --prefix chrome test` — URL and tab rules, default-tab choice, the activity log, the
  manifest, and that the manifest `key` gives the id the runtime allows.
