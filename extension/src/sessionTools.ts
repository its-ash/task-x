import * as vscode from 'vscode';
import { SECTION } from './config.js';
import type { AgentState, ApprovalChoice, ApprovalsResult, ExportResult, RestoreResult, SetApprovalResult, Snapshot, TimelineTask, Turn } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';
import { policy } from './runParams.js';

/**
 * Screens for the runtime's session features that the terminal client has as slash commands:
 * HTML export (`/export`), the workspace timeline (`/timeline`, `/restore`), tool approvals
 * (`/approvals`), and the agents at work (`agent/active`).
 */

type Client = () => Promise<JsonRpcClient>;

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));
const when = (ms: number) => (ms ? new Date(ms).toLocaleString() : '');
const firstLine = (s: string, max = 80) => {
  const l = s.trim().split('\n')[0] ?? '';
  return l.length > max ? `${l.slice(0, max - 1)}…` : l;
};

async function guard(what: string, f: () => Promise<void>): Promise<void> {
  try {
    await f();
  } catch (e) {
    void vscode.window.showErrorMessage(`task-x: ${what} failed: ${message(e)}`);
  }
}

/**
 * Exports turns of a task as one HTML page (`agent/export`). With more than one turn it asks where the
 * export starts and ends (Esc on the end keeps the last); then it offers to open the page.
 */
export function exportHtml(client: Client, taskId: string): Promise<void> {
  return guard('HTML export', async () => {
    const c = await client();
    const { turns } = await c.request<{ turns: Turn[] }>('agent/turns', { taskId });
    let from: number | undefined;
    let to: number | undefined;
    if (turns.length > 1) {
      const items = (list: Turn[]) => list.map((t) => ({ label: `${t.n}`, description: firstLine(t.prompt) || '(no prompt)', detail: when(t.createdMs), n: t.n }));
      const all = { label: '$(list-flat) All turns', description: `1 to ${turns.length}`, n: 0 };
      const start = await vscode.window.showQuickPick([all, ...items(turns)], { title: 'task-x: Export as HTML — start at which turn?', matchOnDescription: true });
      if (!start) return;
      if (start.n) {
        from = start.n;
        const rest = turns.filter((t) => t.n >= start.n).reverse();
        const end = rest.length > 1 ? await vscode.window.showQuickPick(items(rest), { title: `task-x: Export as HTML — end at which turn? (from ${start.n}; Esc: the last)`, matchOnDescription: true }) : undefined;
        to = end?.n ?? turns[turns.length - 1].n;
      }
    }
    const r = await vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title: 'task-x: exporting the chat as HTML…' }, () =>
      c.request<ExportResult>('agent/export', { taskId, from, to }),
    );
    const uri = vscode.Uri.file(r.path);
    const span = r.from === r.to ? `turn ${r.from}` : `turns ${r.from}–${r.to}`;
    const pick = await vscode.window.showInformationMessage(`task-x: exported ${span} to ${vscode.workspace.asRelativePath(uri)} (${Math.ceil(r.bytes / 1024)} KB).`, 'Open in Browser', 'Reveal File');
    if (pick === 'Open in Browser') await vscode.env.openExternal(uri);
    else if (pick === 'Reveal File') await vscode.commands.executeCommand('revealFileInOS', uri);
  });
}

const STATUS_ICON: Record<Snapshot['files'][number]['status'], string> = { added: 'A', modified: 'M', deleted: 'D', typeChanged: 'T' };

/**
 * The workspace timeline (`timeline/*`): pick a task (or the given one), then a snapshot, then view
 * its changes or restore the workspace to it. A restore saves the current state first.
 */
export function showTimeline(client: Client, taskId?: string, step?: number): Promise<void> {
  return guard('timeline', async () => {
    const c = await client();
    let id = taskId;
    if (!id) {
      const { tasks } = await c.request<{ tasks: TimelineTask[]; keep: number }>('timeline/tasks');
      if (!tasks.length) return void vscode.window.showInformationMessage('task-x: no timeline yet. Each task leaves snapshots of the workspace as it works.');
      const pick = await vscode.window.showQuickPick(
        tasks.map((t) => ({ label: firstLine(t.lastLabel) || t.taskId, description: `${t.snapshots} snapshot${t.snapshots === 1 ? '' : 's'}`, detail: `${when(t.lastMs)} · ${t.taskId}`, id: t.taskId })),
        { title: 'task-x: Timeline — which task?' },
      );
      if (!pick) return;
      id = pick.id;
    }
    const { snapshots } = await c.request<{ snapshots: Snapshot[] }>('timeline/list', { taskId: id });
    if (!snapshots.length) return void vscode.window.showInformationMessage('task-x: this task has no timeline (it made no snapshots, or it is older than the last 10 tasks).');
    let snap = step != null ? snapshots.find((s) => s.step === step) : undefined;
    if (step != null && !snap) return void vscode.window.showWarningMessage(`task-x: no step ${step} in this timeline (0 to ${snapshots[snapshots.length - 1].step}).`);
    if (!snap) {
      const pick = await vscode.window.showQuickPick(
        [...snapshots].reverse().map((s) => ({
          label: `Step ${s.step}`,
          description: s.label,
          detail: s.base ? `${when(s.timeMs)} · the workspace before the task` : `${when(s.timeMs)} · ${s.files.length ? s.files.slice(0, 6).map((f) => `${STATUS_ICON[f.status]} ${f.path}`).join(', ') + (s.files.length > 6 ? ` +${s.files.length - 6}` : '') : 'no changes'}`,
          snap: s,
        })),
        { title: 'task-x: Timeline — pick a snapshot', matchOnDescription: true, matchOnDetail: true },
      );
      if (!pick) return;
      snap = pick.snap;
    }
    const action = step != null ? 'restore' : (
      await vscode.window.showQuickPick(
        [
          { label: '$(diff) View changes', description: snap.base ? 'the first snapshot has none' : 'against the snapshot before it', id: 'diff' },
          { label: '$(history) Restore the workspace to this step', description: 'the current state is saved first, so this can be undone', id: 'restore' },
        ],
        { title: `task-x: Step ${snap.step} · ${snap.label}` },
      )
    )?.id;
    if (action === 'diff') {
      const d = await c.request<{ diff: string; truncated: boolean }>('timeline/diff', { taskId: id, snapshot: snap.id });
      const content = d.diff ? d.diff + (d.truncated ? '\n… (cut at 400,000 characters)\n' : '') : 'No changes in this snapshot.\n';
      await vscode.window.showTextDocument(await vscode.workspace.openTextDocument({ language: 'diff', content }), { preview: true });
    } else if (action === 'restore') {
      const ok = await vscode.window.showWarningMessage(
        `Restore the workspace to step ${snap.step} (${snap.label})?`,
        { modal: true, detail: 'Files are written back and files added since are removed. Ignored and large files stay. The current state is saved as a snapshot first.' },
        'Restore',
      );
      if (ok !== 'Restore') return;
      const r = await c.request<RestoreResult>('timeline/restore', { taskId: id, snapshot: snap.id });
      const back = await vscode.window.showInformationMessage(`task-x: restored step ${r.step}; ${r.files.length} file${r.files.length === 1 ? '' : 's'} changed.`, 'Undo Restore');
      if (back === 'Undo Restore') {
        await c.request<RestoreResult>('timeline/restore', { taskId: id, snapshot: r.backup });
        void vscode.window.showInformationMessage('task-x: the workspace is back as it was before the restore.');
      }
    }
  });
}

const CHOICES: { label: string; description: string; choice: ApprovalChoice }[] = [
  { label: '$(check) Allow', description: 'runs without asking', choice: 'allow' },
  { label: '$(question) Ask', description: 'the edit mode decides', choice: 'ask' },
  { label: '$(circle-slash) Deny', description: 'never runs', choice: 'deny' },
];
const DECISION_ICON: Record<ApprovalChoice, string> = { allow: '$(check)', ask: '$(question)', deny: '$(circle-slash)' };
const WHY: Record<ApprovalsResult['tools'][number]['why'], string> = { allowed: 'allow list', denied: 'deny list', session: 'allowed for this session', mode: 'edit mode' };

/** Saves `task-x.agent.allowTools` / `denyTools` where they are set now (the workspace or the user settings). */
async function saveLists(allow: string[], deny: string[]): Promise<void> {
  const cfg = vscode.workspace.getConfiguration(SECTION);
  for (const [key, value] of [['agent.allowTools', allow], ['agent.denyTools', deny]] as const) {
    const i = cfg.inspect<string[]>(key);
    const target = i?.workspaceFolderValue !== undefined ? vscode.ConfigurationTarget.WorkspaceFolder : i?.workspaceValue !== undefined ? vscode.ConfigurationTarget.Workspace : vscode.ConfigurationTarget.Global;
    await cfg.update(key, value, target);
  }
}

/**
 * Tool approvals (`tools/approvals`, `tools/setApproval`): every tool and MCP server with what it does
 * now; picking one sets it to allow, ask, or deny, saved in `task-x.agent.allowTools` / `denyTools`.
 */
export function manageApprovals(client: Client): Promise<void> {
  return guard('approvals', async () => {
    const c = await client();
    for (;;) {
      const p = policy();
      const r = await c.request<ApprovalsResult>('tools/approvals', { policy: p });
      type Item = vscode.QuickPickItem & { target?: string };
      const items: Item[] = [];
      if (r.servers.length) {
        items.push({ label: 'MCP servers', kind: vscode.QuickPickItemKind.Separator });
        for (const s of r.servers)
          items.push({
            label: `$(server) ${s.name}`,
            description: s.allowListed ? 'all allowed' : s.denyListed ? 'all denied' : `${s.allowed} allowed · ${s.asks} ask · ${s.denied} denied`,
            detail: `${s.tools} tools · sets ${s.pattern}`,
            target: s.pattern,
          });
      }
      let server: string | undefined = '';
      for (const t of r.tools) {
        if ((t.server ?? undefined) !== server) {
          server = t.server ?? undefined;
          items.push({ label: server ? `${server} tools` : 'task-x tools', kind: vscode.QuickPickItemKind.Separator });
        }
        items.push({ label: `${DECISION_ICON[t.decision]} ${t.name}`, description: `${t.decision} · ${WHY[t.why]}${t.rule && t.rule !== t.name ? ` (${t.rule})` : ''}`, detail: t.permission, target: t.name });
      }
      const pick = await vscode.window.showQuickPick(items, { title: `task-x: Tool approvals (edit mode: ${p.editMode ?? 'ask'})`, placeHolder: 'Pick a tool or a server to set it to allow, ask, or deny', matchOnDescription: true });
      if (!pick?.target) return;
      const choice = await vscode.window.showQuickPick(CHOICES, { title: `task-x: ${pick.target}` });
      if (!choice) continue;
      // Only the saved lists go back: tools switched off in the chat's Tools chip stay out of the settings.
      const cfg = vscode.workspace.getConfiguration(SECTION);
      const saved = (k: string) => cfg.get<string[]>(k, []).filter((x) => typeof x === 'string');
      const res = await c.request<SetApprovalResult>('tools/setApproval', { policy: { ...p, allow: saved('agent.allowTools'), deny: saved('agent.denyTools') }, tool: pick.target, choice: choice.choice });
      await saveLists(res.allow, res.deny);
      if (res.coveredBy) void vscode.window.showWarningMessage(`task-x: ${pick.target} is set to ${choice.choice}, but ${res.coveredBy} still decides for it.`);
    }
  });
}

/** The agents of this runtime (`agent/active`): leads and subagents, running ones first. */
export function showActiveAgents(client: Client, open?: (taskId: string) => unknown): Promise<void> {
  return guard('active agents', async () => {
    const { agents } = await (await client()).request<{ agents: AgentState[]; running: number }>('agent/active');
    if (!agents.length) return void vscode.window.showInformationMessage('task-x: no agents in the last 10 minutes.');
    const pick = await vscode.window.showQuickPick(
      agents.map((a) => ({
        label: `${'  '.repeat(a.depth)}${a.running ? '$(sync~spin)' : '$(circle-outline)'} ${a.name}${a.agent ? ` (${a.agent})` : a.role ? ` (${a.role})` : ''}`,
        description: `${a.status}${a.currentTool ? ` · ${a.currentTool}` : ''}`,
        detail: `${firstLine(a.title)} · ${a.model ?? ''} · turn ${a.iteration}, ${a.toolCalls} tools, ${a.tokens.toLocaleString()} tokens`,
        id: a.parentId ? undefined : a.taskId,
      })),
      { title: 'task-x: Agents', matchOnDescription: true, matchOnDetail: true },
    );
    if (pick?.id && open) await open(pick.id);
  });
}
