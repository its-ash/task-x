/**
 * MCP elicitation (#15): a server asks the user for input while it works (`mcp/elicit` from the
 * runtime). VS Code shows the server's message, then one input per field of the requested schema
 * (text, number, yes/no, or a pick from the allowed values). Escape cancels; "Decline" says no.
 */
import * as vscode from 'vscode';

interface Field {
  type?: string;
  title?: string;
  description?: string;
  enum?: unknown[];
  enumNames?: string[];
  format?: string;
  minimum?: number;
  maximum?: number;
  minLength?: number;
  maxLength?: number;
  default?: unknown;
}

type Answer = { action: 'accept'; content: Record<string, unknown> } | { action: 'decline' } | { action: 'cancel' };

const MAX_FIELDS = 20;

async function ask(server: string, key: string, f: Field, required: boolean): Promise<{ value?: unknown; cancel?: boolean }> {
  const title = `${server}: ${f.title ?? key}${required ? '' : ' (optional)'}`;
  if (Array.isArray(f.enum) && f.enum.length) {
    const items = f.enum.map((v, i) => ({ label: f.enumNames?.[i] ?? String(v), value: v }));
    const pick = await vscode.window.showQuickPick(required ? items : [{ label: '(skip)', value: undefined }, ...items], { title, placeHolder: f.description, ignoreFocusOut: true });
    return pick ? { value: pick.value } : { cancel: true };
  }
  if (f.type === 'boolean') {
    const pick = await vscode.window.showQuickPick([{ label: 'Yes', value: true }, { label: 'No', value: false }], { title, placeHolder: f.description, ignoreFocusOut: true });
    return pick ? { value: pick.value } : { cancel: true };
  }
  const number = f.type === 'number' || f.type === 'integer';
  const v = await vscode.window.showInputBox({
    title,
    prompt: f.description,
    value: f.default !== undefined ? String(f.default) : undefined,
    password: /password|secret|token/i.test(key) || f.format === 'password',
    ignoreFocusOut: true,
    validateInput: (s) => {
      if (!s && !required) return undefined;
      if (!s) return 'A value is needed.';
      if (number) {
        const n = Number(s);
        if (!Number.isFinite(n) || (f.type === 'integer' && !Number.isInteger(n))) return f.type === 'integer' ? 'Type a whole number.' : 'Type a number.';
        if (f.minimum !== undefined && n < f.minimum) return `At least ${f.minimum}.`;
        if (f.maximum !== undefined && n > f.maximum) return `At most ${f.maximum}.`;
      } else {
        if (f.minLength !== undefined && s.length < f.minLength) return `At least ${f.minLength} characters.`;
        if (f.maxLength !== undefined && s.length > f.maxLength) return `At most ${f.maxLength} characters.`;
        if (f.format === 'email' && !/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(s)) return 'Type an email address.';
        if (f.format === 'uri' && !/^[a-z][\w+.-]*:\S+$/i.test(s)) return 'Type a URL.';
      }
      return undefined;
    },
  });
  if (v === undefined) return { cancel: true };
  if (!v) return {};
  return { value: number ? Number(v) : v };
}

export async function elicit(p: unknown): Promise<Answer> {
  const { server, params } = (p ?? {}) as { server?: unknown; params?: { message?: unknown; requestedSchema?: { properties?: Record<string, Field>; required?: string[] } } };
  const name = typeof server === 'string' ? server : 'MCP server';
  const message = typeof params?.message === 'string' ? params.message.slice(0, 1_000) : 'The server needs more information.';
  const props = Object.entries(params?.requestedSchema?.properties ?? {}).slice(0, MAX_FIELDS);
  const required = new Set(params?.requestedSchema?.required ?? []);
  const go = await vscode.window.showInformationMessage(`MCP server "${name}" asks: ${message}`, { modal: true, detail: props.length ? `It needs: ${props.map(([k, f]) => f.title ?? k).join(', ')}.` : undefined }, 'Answer', 'Decline');
  if (go === 'Decline') return { action: 'decline' };
  if (go !== 'Answer') return { action: 'cancel' };
  const content: Record<string, unknown> = {};
  for (const [key, f] of props) {
    const r = await ask(name, key, f, required.has(key));
    if (r.cancel) return { action: 'cancel' };
    if (r.value !== undefined) content[key] = r.value;
  }
  return { action: 'accept', content };
}
