// Model lists for the provider dialog and the chat's model picker. No `vscode` import: plain Node tests it.
import { execFile } from 'node:child_process';
import { homedir } from 'node:os';
import path from 'node:path';
import type { ModelInfo, ProviderConfig } from './protocol.js';

/** How long a listed set of models is reused before the picker asks the endpoint again. */
export const MODEL_LIST_TTL_MS = 5 * 60_000;
const CLI_TIMEOUT_MS = 5_000;
const MAX_CLI_OUTPUT = 1024 * 1024;

/** Model names from `ollama list` output: the first column of every row after the header. */
export function parseOllamaList(out: string): string[] {
  const names = out
    .split('\n')
    .slice(1)
    .map((l) => l.trim().split(/\s+/)[0])
    .filter((n): n is string => !!n && /^[\w.:/@-]+$/.test(n));
  return [...new Set(names)];
}

/** `ollama` on PATH, then the usual install folders (VS Code started from the Dock has a short PATH). */
function ollamaCandidates(): string[] {
  if (process.platform === 'win32') return ['ollama.exe', path.join(process.env.LOCALAPPDATA ?? '', 'Programs', 'Ollama', 'ollama.exe')];
  return ['ollama', '/opt/homebrew/bin/ollama', '/usr/local/bin/ollama', path.join(homedir(), '.local', 'bin', 'ollama')];
}

type Run = (cmd: string, args: string[]) => Promise<string>;
const run: Run = (cmd, args) =>
  new Promise((resolve, reject) =>
    execFile(cmd, args, { timeout: CLI_TIMEOUT_MS, maxBuffer: MAX_CLI_OUTPUT, windowsHide: true }, (err, stdout) => (err ? reject(err) : resolve(stdout))),
  );

/** The models the Ollama CLI has installed, or undefined when the CLI is not found. */
export async function ollamaCliModels(exec: Run = run): Promise<string[] | undefined> {
  for (const cmd of ollamaCandidates()) {
    try {
      return parseOllamaList(await exec(cmd, ['list']));
    } catch {
      // Not at this path, or the CLI failed; try the next one.
    }
  }
  return undefined;
}

export interface Listed {
  models: ModelInfo[];
  /** Why the list is incomplete or comes from somewhere else. */
  note?: string;
  error?: string;
}

/**
 * Lists a provider's models. When an Ollama server does not answer, the installed models come from
 * `ollama list` instead, so the user sees them and learns to start the server.
 */
export async function listModels(request: (p: ProviderConfig) => Promise<ModelInfo[]>, provider: ProviderConfig, cli = () => ollamaCliModels()): Promise<Listed> {
  try {
    return { models: await request(provider) };
  } catch (e) {
    const error = e instanceof Error ? e.message : String(e);
    const names = provider.kind === 'ollama' ? await cli() : undefined;
    if (!names) return { models: [], error };
    const note = names.length
      ? 'The Ollama server is not answering, so these models come from "ollama list". Start the server ("ollama serve" or the Ollama app) to use them.'
      : 'Ollama is installed, but no models are pulled yet. Run "ollama pull <model>", for example "ollama pull qwen3".';
    return { models: names.map((id) => ({ id, capabilities: [] })), note, error };
  }
}

/** "128K", "200K", "1M": 1024-based when the size is a power-of-two multiple, else 1000-based. */
export function formatTokens(n: number | undefined): string {
  if (!n || !Number.isFinite(n) || n <= 0) return '';
  if (n >= 1_000_000) return `${+(n / 1_000_000).toFixed(1)}M`;
  return n % 1024 === 0 ? `${n / 1024}K` : `${Math.round(n / 1000)}K`;
}

const CAPS = ['tools', 'vision', 'thinking'];
const claude = (id: string, details: string, contextWindow = 1_000_000): ModelInfo => ({ id, contextWindow, capabilities: CAPS, details });

/** Claude Code's aliases (the CLI picks the newest model of each family), then pinned models, newest first. */
export const CLAUDE_MODEL_INFO: ModelInfo[] = [
  claude('default', 'Claude Code default'),
  claude('fable', 'Newest Fable · most capable'),
  claude('opus', 'Newest Opus'),
  claude('sonnet', 'Newest Sonnet · balanced'),
  claude('haiku', 'Newest Haiku · fastest'),
  claude('opusplan', 'Opus plans, Sonnet works'),
  claude('claude-fable-5-1', 'Claude Fable 5.1'),
  claude('claude-opus-5-5', 'Claude Opus 5.5'),
  claude('claude-sonnet-5-5', 'Claude Sonnet 5.5'),
  claude('claude-haiku-5-5', 'Claude Haiku 5.5'),
  claude('claude-fable-5', 'Claude Fable 5'),
  claude('claude-opus-5', 'Claude Opus 5'),
  claude('claude-sonnet-5', 'Claude Sonnet 5'),
  claude('claude-opus-4-8', 'Claude Opus 4.8'),
  claude('claude-opus-4-7', 'Claude Opus 4.7'),
  claude('claude-opus-4-6', 'Claude Opus 4.6'),
  claude('claude-sonnet-4-6', 'Claude Sonnet 4.6'),
  claude('claude-haiku-4-5', 'Claude Haiku 4.5', 200_000),
];

/** The same model: an Ollama name without a tag means `:latest`. */
export const sameModel = (a: string, b: string) => a === b || a === `${b}:latest` || b === `${a}:latest`;

/**
 * The models to show for a provider. The saved model is added while the list is unknown (not
 * loaded yet, or the listing failed), so it can still be picked. A `complete` list shows only
 * what the endpoint has: a model that is not installed must not look installed.
 */
export function withSaved(list: ModelInfo[], saved: string | undefined, complete = false): ModelInfo[] {
  if (!saved || complete || list.some((m) => sameModel(m.id, saved))) return [...list];
  return [{ id: saved, capabilities: [] }, ...list];
}

/** Says that the saved model is missing from a complete list, and how to get it for Ollama. */
export function missingNote(list: ModelInfo[], saved: string | undefined, local: boolean): string | undefined {
  if (!saved || list.some((m) => sameModel(m.id, saved))) return undefined;
  return local ? `"${saved}" is not installed. Run "ollama pull ${saved}" to use it, or pick an installed model.` : `"${saved}" is not in this endpoint's model list.`;
}
