import * as vscode from 'vscode';
import { BackgroundTasks } from './background.js';
import { ChatViewProvider, ORIGINAL_SCHEME, OriginalContent, describeError } from './chatView.js';
import { runBestOf } from './compareView.js';
import { parseFindings } from './findings.js';
import { GraphPanel } from './graphView.js';
import { claudeSignIn, claudeSignOut } from './claudeCode.js';
import { LogView, levelOf } from './logView.js';
import { runtimeMcpServers, setExtensionServers } from './mcpSources.js';
import { type LmToolsHost, lmToolsServer, startLmToolsHost } from './lmToolsMcp.js';
import { plugins } from './catalog.js';
import { InlineReview } from './inlineReview.js';
import { registerVscodeFeatures } from './vscodeFeatures.js';
import { registerEditorCommands } from './editorCommands.js';
import { SECTION, type ProviderRef, activeRef, deleteApiKey, hasApiKey, initChoice, isProviderRef, migrateSettings, modelFor, providerConfig, providerRefs, refLabel, refNeedsKey, sandboxOn, setProvider, settings, storeApiKey } from './config.js';
import { ProviderDialog } from './providerDialog.js';
import type { TestReport } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';
import { RuntimeProcess, findRuntime } from './runtime.js';
import { DEFAULT_CDP_ENDPOINT, findBrowser, launchBrowser, normalizeCdpEndpoint } from './browserBridge.js';
import { type BrowserHost, startBrowserHost } from './vscodeBrowser.js';
import { WorkspaceSync, renderRetrieval, renderSummary } from './workspace.js';

let runtime: RuntimeProcess | undefined;
let starting: Promise<RuntimeProcess> | undefined;
let browserHost: BrowserHost | undefined;

export function activate(ctx: vscode.ExtensionContext): void {
  initChoice(ctx.workspaceState);
  const settingKeys = Object.keys(ctx.extension.packageJSON.contributes?.configuration?.properties ?? {}).map((k) => k.slice(SECTION.length + 1));
  void migrateSettings(settingKeys).then((n) => {
    if (n) void vscode.window.showInformationMessage(`task-x moved ${n} setting(s) from "task-x." to "task-x.".`);
  });
  const raw = vscode.window.createOutputChannel('task-x', { log: true });
  const logs = new LogView(ctx, () => raw.show(true));
  // Every Output channel line also goes to the Logs panel.
  const out = new Proxy(raw, {
    get(target, key, recv) {
      const v = Reflect.get(target, key, recv);
      if (typeof key !== 'string' || !['trace', 'debug', 'info', 'warn', 'error'].includes(key)) return typeof v === 'function' ? v.bind(target) : v;
      return (msg: string | Error, ...args: unknown[]) => {
        logs.add(key as 'info', 'extension', [msg instanceof Error ? msg.message : String(msg), ...args.map(String)].join(' '));
        return (v as (...a: unknown[]) => void).call(target, msg, ...args);
      };
    },
  });
  const version = String(ctx.extension.packageJSON.version ?? '0.0.0');
  const sync = new WorkspaceSync(out);
  // Extensions (hooks, MCP) load after the workspace so `.anvil/*.json` can be read when trusted.
  let lmHost: LmToolsHost | undefined;
  const configureExtensions = async (rt: RuntimeProcess) => {
    const c = vscode.workspace.getConfiguration(SECTION);
    const workspaceTrusted = vscode.workspace.isTrusted;
    try {
      const h = await rt.client.request<{ hooks: number; workspaceHooks: number }>('config/hooks', { hooks: c.get('hooks', []), workspaceTrusted });
      if (h.hooks) out.info(`hooks: ${h.hooks} (${h.workspaceHooks} from .anvil/hooks.json)`);
    } catch (e) {
      vscode.window.showErrorMessage(`task-x hooks: ${describeError(e)}`);
    }
    try {
      // #14: other extensions' language-model tools, as the `vscode` MCP server.
      if (!lmHost) {
        lmHost = await startLmToolsHost((l) => out.warn(l)).catch((e) => {
          out.warn(`VS Code tools server not started: ${describeError(e)}`);
          return undefined;
        });
        if (lmHost) ctx.subscriptions.push(lmHost);
      }
      setExtensionServers(lmToolsServer(lmHost));
      // Refreshes the MCP servers of installed Claude Code plugins (empty without the CLI).
      await plugins();
      const { servers, notes } = runtimeMcpServers(ctx);
      for (const n of notes) out.info(`MCP import: ${n}`);
      const m = await rt.client.request<{ servers: { name: string; running: boolean; tools: string[]; error?: string }[] }>('mcp/configure', { servers, workspaceTrusted });
      for (const sv of m.servers) sv.error ? out.warn(`MCP ${sv.name}: ${sv.error}`) : out.info(`MCP ${sv.name}: ${sv.tools.length} tools`);
    } catch (e) {
      vscode.window.showErrorMessage(`task-x MCP: ${describeError(e)}`);
    }
  };
  const openWorkspace = (rt: RuntimeProcess) =>
    sync
      .open(rt.client)
      .catch((e) => {
        out.error(`workspace indexing failed: ${describeError(e)}`);
        return undefined;
      })
      .finally(() => configureExtensions(rt));

  const ensureRuntime = (): Promise<RuntimeProcess> => {
    if (runtime?.running) return Promise.resolve(runtime);
    starting ??= (async () => {
      const bin = findRuntime(ctx.extensionPath, settings().runtimePath);
      out.info(`starting runtime: ${bin}`);
      if (!browserHost) {
        browserHost = await startBrowserHost((l) => out.warn(l)).catch((e) => {
          out.warn(`VS Code browser bridge not started: ${describeError(e)}`);
          return undefined;
        });
        if (browserHost) ctx.subscriptions.push(browserHost);
      }
      const { runtime: rt, info } = await RuntimeProcess.start(bin, {
        vscodeBrowser: browserHost && { url: browserHost.url, token: browserHost.token },
        clientVersion: version,
        dataDir: ctx.globalStorageUri.fsPath,
        // Commands run in the OS sandbox unless the user turned it off: writes stay inside the workspace.
        sandbox: sandboxOn(),
        cdpEndpoint: settings().cdpEndpoint || process.env.TASK_X_CDP_ENDPOINT,
        log: (l) => {
          raw.info(l);
          logs.add(levelOf(l), 'runtime', l);
        },
        onExit: (code, signal) => {
          out.warn(`runtime exited (code=${code}, signal=${signal})`);
          if (runtime === rt) runtime = undefined;
        },
      });
      out.info(`runtime ready: ${info.serverInfo.name} ${info.serverInfo.version}, protocol ${info.protocolVersion}`);
      runtime = rt;
      void openWorkspace(rt);
      return rt;
    })().finally(() => (starting = undefined));
    return starting;
  };
  const client = async (): Promise<JsonRpcClient> => (await ensureRuntime()).client;

  const originals = new OriginalContent();
  const chat = new ChatViewProvider(ctx, client, () => ProviderDialog.show(ctx, client), originals, (level, msg) => logs.add(level, 'agent', msg));
  const bg = new BackgroundTasks(ctx.secrets, client, chat, (level, msg) => logs.add(level, 'agent', msg));
  chat.foreignApproval = (req) => bg.approve(req);
  chat.onBusyChange = (busy) => bg.setChatBusy(busy);
  // Review findings (an anvil-findings block in the answer) appear in the Problems panel.
  const findings = vscode.languages.createDiagnosticCollection('task-x-review');
  // #4: the pending edits inside the editor, with Keep / Undo per file and Undo per change.
  const review = new InlineReview({ keep: (rel) => chat.settle('keep', [rel]), undoFile: (rel) => chat.settle('revert', [rel]), diff: (rel) => chat.diffFile(rel) });
  chat.onPending = (files) => review.update(files);
  ctx.subscriptions.push(review);
  // #5, #6, #8, #13, #16, #17: quick fixes, commit messages, terminal, tests, review comments, GitHub.
  const features = registerVscodeFeatures(ctx, chat, client);
  ctx.subscriptions.push(...features.disposables);
  chat.onSummary = (s) => {
    const root = WorkspaceSync.root();
    if (!root || !s.text.includes('```anvil-findings')) return;
    findings.clear();
    const byFile = new Map<string, vscode.Diagnostic[]>();
    const found = parseFindings(s.text);
    features.showFindings(root, found);
    for (const f of found) {
      const sev = { error: vscode.DiagnosticSeverity.Error, warning: vscode.DiagnosticSeverity.Warning, info: vscode.DiagnosticSeverity.Information }[f.severity];
      const d = new vscode.Diagnostic(new vscode.Range(f.line - 1, 0, f.line - 1, Number.MAX_SAFE_INTEGER), f.message, sev);
      d.source = 'task-x review';
      byFile.set(f.path, [...(byFile.get(f.path) ?? []), d]);
    }
    for (const [path, list] of byFile) findings.set(vscode.Uri.joinPath(vscode.Uri.file(root), ...path.split('/')), list);
    if (byFile.size) out.info(`review: ${[...byFile.values()].flat().length} findings in ${byFile.size} files`);
  };
  // Indexing status: a status bar item and the chat's index button; both open the project graph.
  const indexItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  indexItem.command = 'task-x.showGraph';
  const showIndex = (st: typeof sync.state) => {
    chat.setIndexState(st);
    if (st.state === 'none') return void indexItem.hide();
    if (st.state === 'indexing') {
      indexItem.text = st.phase === 'graph' ? '$(sync~spin) task-x: building graph' : `$(sync~spin) task-x: indexing ${st.files.toLocaleString()} files`;
      indexItem.tooltip = `Indexing: ${st.files} files seen, ${st.parsed} parsed (${(st.elapsedMs / 1000).toFixed(1)} s)`;
    } else if (st.state === 'ready') {
      indexItem.text = `$(type-hierarchy) ${st.files.toLocaleString()} files`;
      const g = st.graph;
      indexItem.tooltip = new vscode.MarkdownString(
        [`**task-x index**: ${st.files.toLocaleString()} files, ${st.symbols.toLocaleString()} symbols`, g ? `Graph: ${g.modules} modules, ${g.importEdges} imports, ${g.referenceEdges} symbol links, ${g.cycles} import cycles` : '', 'Click to open the project graph.']
          .filter(Boolean)
          .join('\n\n'),
      );
    } else {
      indexItem.text = '$(warning) task-x: index failed';
      indexItem.tooltip = st.error;
    }
    indexItem.show();
  };
  sync.onState(showIndex);
  chat.onShowGraph = () => GraphPanel.show(ctx, client, sync);
  // The chips in the chat input follow the active editor and selection.
  let editorTimer: NodeJS.Timeout | undefined;
  const editorChanged = () => {
    clearTimeout(editorTimer);
    editorTimer = setTimeout(() => chat.postEditor(), 120);
  };
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  status.command = 'task-x.selectProvider';
  const refresh = () => {
    const ref = activeRef();
    status.text = `$(hubot) ${modelFor(ref) || refLabel(ref)}`;
    status.tooltip = `task-x: ${refLabel(ref)} — click to change provider or model`;
    status.show();
    chat.refreshStatus();
  };
  refresh();
  // Start early when a folder is open so the index is ready before the first request.
  if (WorkspaceSync.root()) ensureRuntime().catch((e) => out.warn(`runtime not started: ${describeError(e)}`));

  const pickKind = async (title: string, preferred = activeRef()): Promise<ProviderRef | undefined> => {
    const items = providerRefs().map((k) => ({ label: refLabel(k), description: k === preferred ? 'current' : undefined, value: k }));
    return (await vscode.window.showQuickPick(items, { title, placeHolder: 'Provider' }))?.value;
  };

  const withCancel = <T>(title: string, fn: (signal: AbortSignal) => Promise<T>) =>
    vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title, cancellable: true }, (_p, token) => {
      const ac = new AbortController();
      token.onCancellationRequested(() => ac.abort());
      return fn(ac.signal);
    });

  const selectProvider = async () => {
    const kind = await pickKind('task-x: Select Provider');
    if (!kind) return;
    let models: string[] = [];
    try {
      const provider = await providerConfig(ctx.secrets, kind, 'probe');
      const rpc = await client();
      models = (await withCancel(`Listing ${refLabel(kind)} models…`, (signal) => rpc.request<{ models: string[] }>('provider/listModels', { provider }, { signal }))).models;
    } catch (e) {
      out.warn(`model listing failed: ${describeError(e)}`);
    }
    const current = modelFor(kind);
    let model: string | undefined;
    if (models.length) {
      const custom = '$(edit) Enter a model id…';
      const pick = await vscode.window.showQuickPick([...models.map((m) => ({ label: m, description: m === current ? 'current' : undefined })), { label: custom }], {
        title: `task-x: ${refLabel(kind)} model`,
        placeHolder: current || 'Model',
      });
      model = pick?.label === custom ? undefined : pick?.label;
      if (!pick) return;
    }
    model ??= await vscode.window.showInputBox({
      title: `task-x: ${refLabel(kind)} model`,
      value: current,
      prompt: models.length ? undefined : 'Could not list models from the endpoint; enter the model id.',
      validateInput: (v) => (v.trim() ? undefined : 'Model id is required'),
    });
    if (!model?.trim()) return;
    await setProvider(kind, model.trim());
    if (refNeedsKey(kind) && !(await hasApiKey(ctx.secrets, kind))) {
      const set = await vscode.window.showInformationMessage(`${refLabel(kind)} needs an API key.`, 'Set API Key');
      if (set) await vscode.commands.executeCommand('task-x.setApiKey', kind);
    }
  };

  const setApiKey = async (arg?: unknown) => {
    const kind = isProviderRef(arg) ? arg : await pickKind('task-x: Set API Key');
    if (!kind) return;
    const key = await vscode.window.showInputBox({
      title: `API key for ${refLabel(kind)}`,
      password: true,
      ignoreFocusOut: true,
      prompt: 'Stored in the operating system keychain via VS Code SecretStorage.',
      validateInput: (v) => (!v.trim() ? 'Key is required' : /\s/.test(v.trim()) ? 'Key must not contain whitespace' : undefined),
    });
    if (!key) return;
    await storeApiKey(ctx.secrets, kind, key.trim());
    vscode.window.showInformationMessage(`Saved API key for ${refLabel(kind)}.`);
  };

  const clearApiKey = async () => {
    const kind = await pickKind('task-x: Clear API Key');
    if (!kind) return;
    await deleteApiKey(ctx.secrets, kind);
    vscode.window.showInformationMessage(`Removed API key for ${refLabel(kind)}.`);
  };

  const testConnection = async () => {
    const kind = activeRef();
    try {
      const provider = await providerConfig(ctx.secrets, kind);
      const rpc = await client();
      const r = await withCancel(`Testing ${refLabel(kind)} (${provider.model})…`, (signal) => rpc.request<TestReport>('provider/test', { provider }, { signal }));
      const avail = r.modelAvailable === false ? ` Model "${provider.model}" was not found in ${r.models.length} listed models.` : '';
      const tools = r.capabilities.toolCalling ? 'tool calling supported' : 'no tool calling';
      const msg = `${refLabel(kind)} reachable in ${r.latencyMs} ms; ${tools}.${avail}`;
      out.info(msg);
      (r.modelAvailable === false ? vscode.window.showWarningMessage : vscode.window.showInformationMessage)(msg);
    } catch (e) {
      vscode.window.showErrorMessage(`task-x connection test failed: ${describeError(e)}`);
    }
  };

  const restartRuntime = async () => {
    chat.cancel();
    await runtime?.stop();
    runtime = undefined;
    try {
      await ensureRuntime();
      vscode.window.showInformationMessage('task-x runtime restarted.');
    } catch (e) {
      vscode.window.showErrorMessage(`task-x runtime failed to start: ${describeError(e)}`);
    }
  };

  const showMarkdown = async (content: string) => {
    const doc = await vscode.workspace.openTextDocument({ language: 'markdown', content });
    await vscode.commands.executeCommand('markdown.showPreview', doc.uri).then(undefined, () => vscode.window.showTextDocument(doc));
  };

  const inspectContext = async () => {
    try {
      const rt = await ensureRuntime();
      const summary = sync.summary ?? (await sync.open(rt.client));
      if (!summary) return void vscode.window.showWarningMessage('Open a folder to inspect its context.');
      await showMarkdown(renderSummary(summary));
    } catch (e) {
      vscode.window.showErrorMessage(`task-x: ${describeError(e)}`);
    }
  };

  const previewContext = async () => {
    const query = await vscode.window.showInputBox({ title: 'task-x: Preview Context', prompt: 'Describe a coding request. task-x shows the files and instructions it would use.' });
    if (!query?.trim()) return;
    try {
      const rt = await ensureRuntime();
      if (!sync.summary) await sync.open(rt.client);
      const active = vscode.window.activeTextEditor?.document.uri;
      const focus = active ? [WorkspaceSync.relative(active)].filter((p): p is string => !!p) : [];
      await showMarkdown(renderRetrieval(query, await sync.retrieve(query.trim(), focus)));
    } catch (e) {
      vscode.window.showErrorMessage(`task-x: ${describeError(e)}`);
    }
  };

  const reindex = async () => {
    try {
      const rt = await ensureRuntime();
      const s = await vscode.window.withProgress({ location: vscode.ProgressLocation.Window, title: 'task-x: indexing' }, () => sync.open(rt.client));
      if (s) vscode.window.showInformationMessage(`task-x indexed ${s.stats.files} files and ${s.stats.symbols} symbols.`);
    } catch (e) {
      vscode.window.showErrorMessage(`task-x: ${describeError(e)}`);
    }
  };

  ctx.subscriptions.push(
    out,
    sync,
    vscode.workspace.registerTextDocumentContentProvider(ORIGINAL_SCHEME, originals),
    vscode.commands.registerCommand('task-x.keepAll', () => chat.settle('keep')),
    vscode.commands.registerCommand('task-x.undoAll', () => chat.settle('revert')),
    vscode.commands.registerCommand('task-x.inspectContext', inspectContext),
    vscode.commands.registerCommand('task-x.previewContext', previewContext),
    vscode.commands.registerCommand('task-x.reindex', reindex),
    vscode.workspace.onDidChangeWorkspaceFolders(() => runtime && void openWorkspace(runtime)),
    vscode.workspace.onDidGrantWorkspaceTrust(() => runtime && void configureExtensions(runtime)),
    (() => {
      const w = vscode.workspace.createFileSystemWatcher('**/{.task-x/hooks.json,.task-x/mcp.json,.anvil/hooks.json,.anvil/mcp.json,.mcp.json,.vscode/mcp.json}');
      const again = () => runtime && void configureExtensions(runtime);
      w.onDidChange(again);
      w.onDidCreate(again);
      w.onDidDelete(again);
      return w;
    })(),
    // Reads every MCP source again (Claude, its plugins, VS Code, Copilot CLI) and restarts the servers.
    vscode.commands.registerCommand('task-x.reloadMcp', async () => configureExtensions(await ensureRuntime())),
    vscode.commands.registerCommand('task-x.mcpStatus', async () => {
      try {
        const rt = await ensureRuntime();
        const st = await rt.client.request<{ servers: { name: string; running: boolean; tools: string[]; error?: string; command: string }[] }>('mcp/status');
        if (!st.servers.length) return void vscode.window.showInformationMessage('task-x: no MCP servers found. Add them in Claude, VS Code (mcp.json), the setting task-x.mcpServers, or .task-x/mcp.json.');
        await vscode.window.showQuickPick(
          st.servers.map((sv) => ({ label: `${sv.running ? '$(pass-filled)' : '$(error)'} ${sv.name}`, description: sv.command, detail: sv.error ?? (sv.tools.join(', ') || 'no tools') })),
          { title: 'task-x: MCP Servers' },
        );
      } catch (e) {
        vscode.window.showErrorMessage(`task-x: ${describeError(e)}`);
      }
    }),
    vscode.commands.registerCommand('task-x.listSkills', async () => {
      try {
        const rt = await ensureRuntime();
        const skills = await rt.client.request<{ name: string; description: string; source: string; triggers: string[] }[]>('skills/list');
        if (!skills.length) return void vscode.window.showInformationMessage('task-x: no skills found. Add Markdown files under .anvil/skills/ or ~/.anvil/skills/.');
        const pick = await vscode.window.showQuickPick(
          skills.map((sk) => ({ label: `/${sk.name}`, description: sk.description, detail: sk.triggers.length ? `triggers: ${sk.triggers.join(', ')}` : sk.source, source: sk.source })),
          { title: 'task-x: Skills (type /name in chat to use one)' },
        );
        if (pick) await vscode.window.showTextDocument(vscode.Uri.file(pick.source));
      } catch (e) {
        vscode.window.showErrorMessage(`task-x: ${describeError(e)}`);
      }
    }),
    status,
    chat,
    vscode.window.registerWebviewViewProvider(ChatViewProvider.viewId, chat, { webviewOptions: { retainContextWhenHidden: true } }),
    vscode.commands.registerCommand('task-x.openChat', () => vscode.commands.executeCommand(`${ChatViewProvider.viewId}.focus`)),
    vscode.commands.registerCommand('task-x.selectProvider', selectProvider),
    vscode.window.registerWebviewViewProvider(LogView.viewId, logs),
    vscode.commands.registerCommand('task-x.showLogs', () => vscode.commands.executeCommand(`${LogView.viewId}.focus`)),
    vscode.commands.registerCommand('task-x.clearLogs', () => logs.clear()),
    vscode.commands.registerCommand('task-x.copySelection', () => chat.copySelection()),
    vscode.commands.registerCommand('task-x.signIn', () => claudeSignIn(() => chat.refreshStatus())),
    vscode.commands.registerCommand('task-x.signOut', () => claudeSignOut(() => chat.refreshStatus())),
    vscode.commands.registerCommand('task-x.addProvider', (kind?: unknown) => ProviderDialog.show(ctx, client, isProviderRef(kind) ? kind : undefined)),
    vscode.commands.registerCommand('task-x.setApiKey', setApiKey),
    vscode.commands.registerCommand('task-x.clearApiKey', clearApiKey),
    vscode.commands.registerCommand('task-x.testConnection', testConnection),
    vscode.commands.registerCommand('task-x.cancel', () => chat.cancel()),
    vscode.commands.registerCommand('task-x.clearChat', () => chat.clear()),
    vscode.commands.registerCommand('task-x.newChat', () => chat.newChat()),
    vscode.commands.registerCommand('task-x.undoLast', () => chat.undoLast()),
    ...registerEditorCommands(chat, client),
    vscode.commands.registerCommand('task-x.restartRuntime', restartRuntime),
    vscode.commands.registerCommand('task-x.openBrowser', async () => {
      // The agent's browser tool drives a Chrome/Edge over CDP. This starts one with a debug port;
      // the endpoint then comes through the environment to the runtime (config.ts exposes it).
      const configured = normalizeCdpEndpoint(settings().cdpEndpoint || DEFAULT_CDP_ENDPOINT);
      const port = Number(new URL(configured).port) || 9222;
      const chrome = findBrowser({ existsSync: (p) => require('node:fs').existsSync(p) });
      if (!chrome) throw new Error('No Chrome, Edge, or Chromium found. Install one, or start it yourself with --remote-debugging-port.');
      launchBrowser(chrome, port);
      void vscode.window.showInformationMessage(`task-x: browser started with a debugging port on ${configured}`);
    }),
    bg,
    findings,
    vscode.commands.registerCommand('task-x.runInBackground', () => bg.start()),
    vscode.commands.registerCommand('task-x.showTasks', () => bg.show()),
    vscode.commands.registerCommand('task-x.bestOf', () => runBestOf(ctx, client, chat, bg)),
    vscode.commands.registerCommand('task-x.clearFindings', () => findings.clear()),
    indexItem,
    vscode.window.onDidChangeActiveTextEditor(editorChanged),
    vscode.window.onDidChangeTextEditorSelection(editorChanged),
    vscode.commands.registerCommand('task-x.showGraph', () => GraphPanel.show(ctx, client, sync)),
    vscode.commands.registerCommand('task-x.showFileGraph', (uri?: vscode.Uri) => {
      const target = uri instanceof vscode.Uri ? uri : vscode.window.activeTextEditor?.document.uri;
      const rel = target && WorkspaceSync.relative(target);
      GraphPanel.show(ctx, client, sync, rel);
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('task-x.engine') || e.affectsConfiguration('task-x.claudeCode')) chat.refreshStatus();
      if (e.affectsConfiguration(SECTION)) {
        if (e.affectsConfiguration(`${SECTION}.customProviders`) || providerRefs().some((k) => e.affectsConfiguration(`${SECTION}.${k}.baseUrl`))) chat.invalidateModels();
        refresh();
      }
      // The sandbox switch applies to the next command at once, without a restart.
      if (e.affectsConfiguration('task-x.sandbox.enabled') && runtime) void runtime.client.request('config/sandbox', { enabled: sandboxOn() }).catch((err) => out.warn(`config/sandbox failed: ${describeError(err)}`));
      if (e.affectsConfiguration('task-x.runtimePath') && runtime) void restartRuntime();
      else if (e.affectsConfiguration('task-x.index.exclude') && runtime) void openWorkspace(runtime);
      else if (['task-x.mcpServers', 'task-x.mcp.import', 'task-x.mcp.disabled', 'task-x.hooks', 'mcp.servers'].some((k) => e.affectsConfiguration(k)) && runtime) void configureExtensions(runtime);
    }),
    ctx.secrets.onDidChange(() => {
      chat.invalidateModels();
      chat.refreshStatus();
    }),
  );
}

export async function deactivate(): Promise<void> {
  await runtime?.stop();
  runtime = undefined;
}
