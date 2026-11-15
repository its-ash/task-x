import * as vscode from 'vscode';
import { portOrigin, portUrl, portWhere, portsStatusText, portsTooltip } from './portsFormat.js';
import type { PortInfo, PortList, PortStopResult } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';

/** How often the status bar re-reads the ports while VS Code has focus. */
const POLL_MS = 5000;

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

type Item = vscode.QuickPickItem & { port?: PortInfo; stopAll?: true };

/**
 * The ports taken by processes task-x started (a dev server, a preview) or that run in the
 * workspace: a status bar item while any is taken, and a list to open each one in the browser or
 * stop its process (`ports/list`, `ports/stop`).
 */
export class PortsView implements vscode.Disposable {
  private readonly item = vscode.window.createStatusBarItem('task-x.ports', vscode.StatusBarAlignment.Right, 98);
  private readonly timer: NodeJS.Timeout;
  private ports: PortInfo[] = [];
  private polling = false;

  /**
   * @param running the client of a runtime that already runs: polling never starts one, and it
   *   does not count as use (an idle runtime still stops).
   * @param getClient starts the runtime when needed, for the list the user opens.
   */
  constructor(
    private readonly running: () => JsonRpcClient | undefined,
    private readonly getClient: () => Promise<JsonRpcClient>,
    private readonly log: (msg: string) => void,
  ) {
    this.item.name = 'task-x Ports';
    this.item.command = 'task-x.showPorts';
    this.timer = setInterval(() => void this.poll(), POLL_MS);
    void this.poll();
  }

  dispose(): void {
    clearInterval(this.timer);
    this.item.dispose();
  }

  /** Re-reads the ports from the running runtime; with none, nothing is shown. */
  async poll(): Promise<void> {
    const client = this.running();
    if (!client) return this.render([]);
    if (this.polling || !vscode.window.state.focused) return;
    this.polling = true;
    try {
      this.render((await client.request<PortList>('ports/list')).ports);
    } catch (e) {
      this.log(`ports/list failed: ${message(e)}`);
    } finally {
      this.polling = false;
    }
  }

  private render(ports: PortInfo[]): void {
    this.ports = ports;
    if (!ports.length) return void this.item.hide();
    this.item.text = portsStatusText(ports);
    this.item.tooltip = new vscode.MarkdownString(portsTooltip(ports));
    this.item.show();
  }

  private async refresh(client: JsonRpcClient): Promise<PortList> {
    const list = await client.request<PortList>('ports/list');
    this.render(list.ports);
    return list;
  }

  /** The list: select a port to open it in the browser; its buttons open or stop it. */
  async show(): Promise<void> {
    let client: JsonRpcClient;
    let list: PortList;
    try {
      client = await this.getClient();
      list = await this.refresh(client);
    } catch (e) {
      return void vscode.window.showErrorMessage(`task-x: listing ports failed: ${message(e)}`);
    }
    if (!list.supported) return void vscode.window.showWarningMessage('task-x: listing ports needs lsof or ss on this machine.');
    if (!list.ports.length) return void vscode.window.showInformationMessage('task-x: no ports are in use by processes task-x started or that run in this workspace.');
    const open: vscode.QuickInputButton = { iconPath: new vscode.ThemeIcon('globe'), tooltip: 'Open in Browser' };
    const stop: vscode.QuickInputButton = { iconPath: new vscode.ThemeIcon('debug-stop'), tooltip: 'Stop This Process' };
    const items = (): Item[] => [
      ...this.ports.map((p) => ({
        label: `$(circle-filled) ${p.port}`,
        description: `occupied · ${p.name} · pid ${p.pid}`,
        detail: `${p.command} · ${portOrigin(p)} · on ${portWhere(p)}`,
        port: p,
        buttons: [open, stop],
      })),
      ...(this.ports.length > 1 ? [{ label: '$(debug-stop) Stop All', description: `${this.ports.length} processes`, stopAll: true as const }] : []),
    ];
    const qp = vscode.window.createQuickPick<Item>();
    qp.title = 'task-x: Ports in Use';
    qp.placeholder = 'Select a port to open it in the browser, or stop it with its button';
    qp.items = items();
    const after = () => {
      if (!this.ports.length) return qp.hide();
      qp.items = items();
    };
    qp.onDidTriggerItemButton(async (e) => {
      const p = e.item.port;
      if (!p) return;
      if (e.button === open) return void vscode.env.openExternal(vscode.Uri.parse(portUrl(p)));
      qp.busy = true;
      await this.stop(client, p);
      qp.busy = false;
      after();
    });
    qp.onDidAccept(async () => {
      const it = qp.selectedItems[0];
      if (it?.port) {
        qp.hide();
        return void vscode.env.openExternal(vscode.Uri.parse(portUrl(it.port)));
      }
      if (!it?.stopAll) return;
      const pids = [...new Set(this.ports.map((p) => p.pid))];
      const ok = await vscode.window.showWarningMessage(`Stop ${pids.length} processes and free their ports?`, { modal: true }, 'Stop All');
      if (ok !== 'Stop All') return;
      qp.busy = true;
      for (const pid of pids) {
        const p = this.ports.find((x) => x.pid === pid);
        if (p) await this.stop(client, p, false);
      }
      qp.busy = false;
      after();
    });
    qp.onDidHide(() => qp.dispose());
    qp.show();
  }

  /** Stops a port's process; one that only runs in the workspace (task-x did not start it) is confirmed first. */
  private async stop(client: JsonRpcClient, p: PortInfo, confirm = true): Promise<void> {
    if (confirm && p.origin === 'workspace') {
      const ok = await vscode.window.showWarningMessage(`Stop ${p.name} (pid ${p.pid}) on port ${p.port}? task-x did not start it; it runs in this workspace.`, { modal: true }, 'Stop');
      if (ok !== 'Stop') return;
    }
    try {
      const r = await client.request<PortStopResult>('ports/stop', { pid: p.pid });
      if (!r.stopped) void vscode.window.showWarningMessage(`task-x: ${p.name} (pid ${p.pid}) did not stop; port ${p.port} is still taken.`);
      else this.log(`stopped ${p.name} (pid ${p.pid}) on port ${p.port}${r.forced ? ' (killed)' : ''}`);
    } catch (e) {
      void vscode.window.showErrorMessage(`task-x: stopping port ${p.port} failed: ${message(e)}`);
    }
    try {
      await this.refresh(client);
    } catch (e) {
      this.log(`ports/list failed: ${message(e)}`);
    }
  }
}
