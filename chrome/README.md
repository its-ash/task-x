# task-x for Chrome

Lets the task-x agent drive your own Chrome (signed in, no debug port) through its `browser`
tool. Loaded unpacked; it is not on the Chrome Web Store.

1. From the repository root: `make chrome-install` (builds and installs the runtime, then
   registers it with every Chromium browser you have).
2. `chrome://extensions` → **Developer mode** → **Load unpacked** → this folder.
3. Click the task-x toolbar button; the side panel should say **Connected**.

The toolbar icon is **green** while task-x is connected and active, and **grey** when it is not
connected or is paused.

task-x may use **every tab** by default; turn off **Let task-x use every tab** in the side panel to
limit it to the tabs it opened and the ones you share (**Share this tab**, or ⌘⇧Y). Pages it opens
appear in a purple **task-x · automated** tab group, and Chrome shows its own "started debugging
this browser" bar while task-x drives a tab. **Pause** stops everything.

Design and protocol: [`docs/chrome-bridge.md`](../docs/chrome-bridge.md). Tests: `npm --prefix chrome test`.
