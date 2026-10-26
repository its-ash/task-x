import * as vscode from 'vscode';
import type { ChatViewProvider } from './chatView.js';
import { describeError } from './chatView.js';
import { FINDINGS_INSTRUCTION } from './findings.js';
import type { ChatMode } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';
import { WorkspaceSync } from './workspace.js';

interface TaskRow {
  id: string;
  title: string;
  status: string;
  chatMode: ChatMode;
  updatedMs: number;
  changedFiles: string[];
}

const where = (ed: vscode.TextEditor) => {
  const rel = WorkspaceSync.relative(ed.document.uri) ?? ed.document.fileName;
  return ed.selection.isEmpty ? rel : `${rel}:${ed.selection.start.line + 1}-${ed.selection.end.line + 1}`;
};

function requireEditor(): vscode.TextEditor | undefined {
  const ed = vscode.window.activeTextEditor;
  if (!ed) vscode.window.showInformationMessage('task-x: open a file first.');
  return ed;
}

/** Context-menu and palette commands. The selection and diagnostics travel with every request. */
export function registerEditorCommands(chat: ChatViewProvider, getClient: () => Promise<JsonRpcClient>): vscode.Disposable[] {
  const cmd = (id: string, fn: () => unknown) => vscode.commands.registerCommand(id, fn);
  const send = (text: string, mode?: ChatMode) => chat.submit(text, mode);

  return [
    cmd('task-x.askAgent', async () => {
      const ed = vscode.window.activeTextEditor;
      const text = await vscode.window.showInputBox({
        title: 'Ask task-x',
        prompt: ed && !ed.selection.isEmpty ? `About the selection in ${where(ed)}` : 'Your request',
        ignoreFocusOut: true,
      });
      if (text?.trim()) await send(text);
    }),
    cmd('task-x.explainSelection', async () => {
      const ed = requireEditor();
      if (ed) await send(`Explain what the selected code in ${where(ed)} does, how it fits into the surrounding code, and anything surprising about it.`);
    }),
    cmd('task-x.fixSelection', async () => {
      const ed = requireEditor();
      if (ed) await send(`Fix the problems in ${where(ed)}. Use the editor diagnostics if present; find the root cause, make the smallest correct change, and validate it.`, 'agent');
    }),
    cmd('task-x.refactorSelection', async () => {
      const ed = requireEditor();
      if (!ed) return;
      const goal = await vscode.window.showInputBox({ title: 'Refactor selection', prompt: 'Goal (optional), e.g. "extract a function" or "remove duplication"' });
      if (goal === undefined) return;
      await send(`Refactor the selected code in ${where(ed)}${goal.trim() ? ` to ${goal.trim()}` : ' for clarity'}. Keep behaviour the same, update all references, and run the tests.`, 'agent');
    }),
    cmd('task-x.generateTests', async () => {
      const ed = requireEditor();
      if (ed) await send(`Write unit tests for ${ed.selection.isEmpty ? 'the code in' : 'the selected code in'} ${where(ed)}. Follow the project's existing test framework, location, and style; cover edge cases; then run the tests and fix failures.`, 'agent');
    }),
    cmd('task-x.reviewChanges', () =>
      send(
        `Review my uncommitted changes (use git_status and git_diff). Report concrete bugs, regressions, security issues, missing tests, and API breaks, each with file:line, severity (high/medium/low), and confidence (confirmed/likely/possible). Do not report style nits as defects. Do not edit files. ${FINDINGS_INSTRUCTION}`,
      ),
    ),
    cmd('task-x.explainError', async () => {
      const ed = requireEditor();
      if (!ed) return;
      const pos = ed.selection.active;
      const diags = vscode.languages.getDiagnostics(ed.document.uri);
      const d = diags.find((x) => x.range.contains(pos)) ?? diags.find((x) => x.severity === vscode.DiagnosticSeverity.Error);
      if (!d) return void vscode.window.showInformationMessage('task-x: no problem found at the cursor.');
      const rel = WorkspaceSync.relative(ed.document.uri) ?? ed.document.fileName;
      await send(`Explain this ${vscode.DiagnosticSeverity[d.severity].toLowerCase()} at ${rel}:${d.range.start.line + 1}${d.source ? ` (${d.source})` : ''} and how to fix it:\n\n${d.message}`);
    }),
    cmd('task-x.continueTask', () => {
      if (!chat.currentTaskId) return void vscode.window.showInformationMessage('task-x: no task to continue. Open one from the task history.');
      return send('Continue the task from where you stopped. Check what is already done before making more changes.', 'agent');
    }),
    cmd('task-x.openTask', async () => {
      try {
        const rpc = await getClient();
        const tasks = await rpc.request<TaskRow[]>('agent/tasks');
        if (!tasks.length) return void vscode.window.showInformationMessage('task-x: no saved tasks yet.');
        const pick = await vscode.window.showQuickPick(
          tasks.map((t) => ({
            label: t.title,
            description: `${t.chatMode} · ${t.status}`,
            detail: `${new Date(t.updatedMs).toLocaleString()}${t.changedFiles.length ? ` · ${t.changedFiles.length} file(s) changed` : ''}`,
            id: t.id,
          })),
          { title: 'task-x: Task History', matchOnDescription: true },
        );
        if (pick) await chat.openTask(pick.id);
      } catch (e) {
        vscode.window.showErrorMessage(`task-x: ${describeError(e)}`);
      }
    }),
  ];
}
