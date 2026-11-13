/**
 * OAuth sign-in for remote MCP servers. The runtime runs the flow (`mcp/authorize`); the extension
 * opens the sign-in page (`mcp/openUrl`), keeps the tokens in SecretStorage (`mcp/tokens`), and
 * sends them back with every `mcp/configure`. Tokens are never logged or written to settings.
 */
import * as vscode from 'vscode';
import type { JsonRpcClient } from './rpc.js';

const KEY = (server: string) => `task-x.mcp.oauth.${server}`;
const NAME = /^[\w.-]{1,64}$/;

export interface McpServerStatus {
  name: string;
  command: string;
  running: boolean;
  tools: string[];
  error?: string | null;
  authRequired?: boolean;
  signedIn?: boolean;
}

const wired = new WeakSet<JsonRpcClient>();

/** Handles the runtime's sign-in requests and token notifications for one client. */
export function wireMcpAuth(client: JsonRpcClient, secrets: vscode.SecretStorage, log: (m: string) => void): void {
  if (wired.has(client)) return;
  wired.add(client);
  client.onRequest('mcp/openUrl', async (p) => {
    const { server, url } = p as { server: string; url: string };
    let uri: vscode.Uri;
    try {
      uri = vscode.Uri.parse(url, true);
    } catch {
      return { opened: false };
    }
    if (uri.scheme !== 'https' && !(uri.scheme === 'http' && /^(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$/.test(uri.authority))) return { opened: false };
    const opened = await vscode.env.openExternal(uri);
    void vscode.window.showInformationMessage(`task-x: sign in to ${server} in your browser.`, 'Copy link').then((pick) => pick && vscode.env.clipboard.writeText(url));
    return { opened };
  });
  client.onNotification('mcp/tokens', (p) => {
    const { server, tokens } = p as { server: string; tokens: unknown };
    if (typeof server !== 'string' || !NAME.test(server) || !tokens || typeof tokens !== 'object') return;
    void secrets.store(KEY(server), JSON.stringify(tokens)).then(() => log(`MCP ${server}: sign-in saved`));
  });
}

/** The servers with the tokens of earlier sign-ins added (remote servers only). */
export async function withTokens<T extends { url?: string }>(servers: Record<string, T>, secrets: vscode.SecretStorage): Promise<Record<string, T & { oauth?: unknown }>> {
  const out: Record<string, T & { oauth?: unknown }> = {};
  for (const [name, s] of Object.entries(servers)) {
    const raw = s && typeof s.url === 'string' && NAME.test(name) ? await secrets.get(KEY(name)) : undefined;
    let oauth: unknown;
    try {
      oauth = raw ? JSON.parse(raw) : undefined;
    } catch {
      oauth = undefined;
    }
    out[name] = oauth ? { ...s, oauth } : s;
  }
  return out;
}

async function pick(client: JsonRpcClient, filter: (s: McpServerStatus) => boolean, title: string): Promise<string | undefined> {
  const { servers } = await client.request<{ servers: McpServerStatus[] }>('mcp/status');
  const list = servers.filter(filter);
  if (!list.length) {
    void vscode.window.showInformationMessage(`task-x: no MCP server to ${title.toLowerCase()}.`);
    return undefined;
  }
  const choice = await vscode.window.showQuickPick(
    list.map((s) => ({ label: s.name, description: s.authRequired ? 'needs sign-in' : s.signedIn ? 'signed in' : s.running ? 'running' : 'not running', detail: s.command })),
    { title: `task-x: ${title}` },
  );
  return choice?.label;
}

/** Signs in to a remote MCP server (the browser opens), then the server starts with the token. */
export async function signIn(client: JsonRpcClient, server?: string): Promise<void> {
  const name = server && NAME.test(server) ? server : await pick(client, (s) => /^https?:/.test(s.command), 'Sign In to MCP Server');
  if (!name) return;
  await vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title: `task-x: signing in to ${name}…`, cancellable: true }, async (_, token) => {
    const ac = new AbortController();
    token.onCancellationRequested(() => ac.abort());
    try {
      const st = await client.request<{ servers: McpServerStatus[] }>('mcp/authorize', { server: name }, { signal: ac.signal });
      const s = st.servers.find((x) => x.name === name);
      void vscode.window.showInformationMessage(`task-x: signed in to ${name}${s?.tools.length ? ` (${s.tools.length} tools)` : ''}.`);
    } catch (e) {
      if (!token.isCancellationRequested) void vscode.window.showErrorMessage(`task-x: sign-in to ${name} failed: ${e instanceof Error ? e.message : String(e)}`);
    }
  });
}

/** Forgets the tokens of a server; `reload` restarts the servers without them. */
export async function signOut(client: JsonRpcClient, secrets: vscode.SecretStorage, reload: () => Promise<unknown>, server?: string): Promise<void> {
  const name = server && NAME.test(server) ? server : await pick(client, (s) => !!s.signedIn, 'Sign Out of MCP Server');
  if (!name) return;
  await secrets.delete(KEY(name));
  await reload();
  void vscode.window.showInformationMessage(`task-x: signed out of ${name}.`);
}
