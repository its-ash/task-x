/**
 * Custom chat modes (#12), in Copilot's format: `.github/chatmodes/<name>.chatmode.md`, and
 * `.task-x/modes/<name>.md`. Front matter `description` and `tools`; the body is the instructions.
 * The chosen mode's instructions go with every message until the user picks another one.
 */
import { readdirSync, readFileSync, statSync } from 'node:fs';
import path from 'node:path';
import { setWindowChoice, windowChoice } from './config.js';
import { WorkspaceSync } from './workspace.js';

const MAX_MODE_BYTES = 32 * 1024;
const MAX_MODES = 50;

export interface ChatModeFile {
  name: string;
  description: string;
  tools: string[];
  instructions: string;
  path: string;
}

function parse(file: string, name: string): ChatModeFile | undefined {
  try {
    if (statSync(file).size > MAX_MODE_BYTES) return undefined;
    const text = readFileSync(file, 'utf8');
    const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/.exec(text);
    const head = m?.[1] ?? '';
    const field = (k: string) => new RegExp(`^${k}:\\s*(.+)$`, 'm').exec(head)?.[1].trim().replace(/^["']|["']$/g, '');
    const tools = (field('tools') ?? '').replace(/^\[|\]$/g, '').split(',').map((t) => t.trim().replace(/^["']|["']$/g, '')).filter(Boolean);
    const instructions = (m ? m[2] : text).trim();
    return instructions ? { name: field('name') || name, description: field('description') ?? '', tools, instructions, path: file } : undefined;
  } catch {
    return undefined;
  }
}

/** The project's custom modes, by name (a later folder wins on the same name). */
export function chatModes(root = WorkspaceSync.root()): ChatModeFile[] {
  if (!root) return [];
  const out = new Map<string, ChatModeFile>();
  const scan = (dir: string, suffix: string) => {
    let names: string[] = [];
    try {
      names = readdirSync(dir).filter((f) => f.endsWith(suffix));
    } catch {
      return;
    }
    for (const f of names) {
      const m = parse(path.join(dir, f), f.slice(0, -suffix.length));
      if (m && out.size < MAX_MODES) out.set(m.name, m);
    }
  };
  scan(path.join(root, '.github', 'chatmodes'), '.chatmode.md');
  scan(path.join(root, '.task-x', 'modes'), '.md');
  return [...out.values()].sort((a, b) => a.name.localeCompare(b.name));
}

export const activeChatMode = (): ChatModeFile | undefined => {
  const name = windowChoice().chatMode;
  return name ? chatModes().find((m) => m.name === name) : undefined;
};

export async function setChatMode(name: string | undefined): Promise<void> {
  await setWindowChoice({ chatMode: name && chatModes().some((m) => m.name === name) ? name : undefined });
}

/** The message with the active mode's instructions in front of it; unchanged without a mode. */
export function withChatMode(text: string): string {
  const m = activeChatMode();
  if (!m) return text;
  const tools = m.tools.length ? `\nUse only these tools in this mode: ${m.tools.join(', ')}.` : '';
  return `<chat_mode name="${m.name}">\n${m.instructions}${tools}\n</chat_mode>\n\n${text}`;
}
