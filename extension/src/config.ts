import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { EFFORTS, type EditMode, type Effort, PROVIDER_KINDS, PROVIDER_LABELS, type ProviderConfig, type ProviderKind, requiresApiKey } from './protocol.js';

export { DEFAULT_BASE_URLS, requiresApiKey } from './protocol.js';

export const SECTION = 'task-x';
const DEFAULT_TIMEOUT_MS = 120_000;
const DEFAULT_MAX_TOKENS = 4_096;
const CUSTOM = 'custom:';
const MAX_CUSTOM = 50;
const MAX_NAME = 60;

/** A named endpoint the user added, such as a second OpenAI-compatible server. */
export interface CustomProvider {
  id: string;
  name: string;
  kind: ProviderKind;
  baseUrl: string;
  model: string;
}

/** A built-in provider kind, or `custom:<id>` for a named endpoint. */
export type ProviderRef = string;

const secretKey = (ref: ProviderRef) => `${SECTION}.apiKey.${ref}`;

export const isProviderKind = (v: unknown): v is ProviderKind => PROVIDER_KINDS.includes(v as ProviderKind);

export function customProviders(): CustomProvider[] {
  const raw = cfg().get<unknown[]>('customProviders', []);
  const seen = new Set<string>();
  const out: CustomProvider[] = [];
  for (const r of Array.isArray(raw) ? raw : []) {
    const c = r as Partial<CustomProvider>;
    if (typeof c?.id !== 'string' || !/^[\w-]{1,40}$/.test(c.id) || seen.has(c.id) || !isProviderKind(c.kind)) continue;
    seen.add(c.id);
    const s = (v: unknown) => (typeof v === 'string' ? v.trim() : '');
    out.push({ id: c.id, name: s(c.name).slice(0, MAX_NAME) || c.id, kind: c.kind, baseUrl: s(c.baseUrl), model: s(c.model) });
  }
  return out.slice(0, MAX_CUSTOM);
}

const customOf = (ref: ProviderRef) => (ref.startsWith(CUSTOM) ? customProviders().find((c) => c.id === ref.slice(CUSTOM.length)) : undefined);

/** Built-in providers. OpenAI-compatible servers are only named endpoints, so a user can add any number of them. */
const BUILTIN_REFS: readonly ProviderKind[] = PROVIDER_KINDS.filter((k) => k !== 'openaiCompatible');

export const isProviderRef = (v: unknown): v is ProviderRef => typeof v === 'string' && (BUILTIN_REFS.includes(v as ProviderKind) || !!customOf(v));

/** Built-in kinds first, then the named endpoints. */
export const providerRefs = (): ProviderRef[] => [...BUILTIN_REFS, ...customProviders().map((c) => CUSTOM + c.id)];

export const refKind = (ref: ProviderRef): ProviderKind => (isProviderKind(ref) ? ref : (customOf(ref)?.kind ?? 'openaiCompatible'));

export const refLabel = (ref: ProviderRef): string => (isProviderKind(ref) ? PROVIDER_LABELS[ref] : (customOf(ref)?.name ?? ref));

export const refNeedsKey = (ref: ProviderRef) => requiresApiKey(refKind(ref));

export function baseUrlFor(ref: ProviderRef): string {
  return isProviderKind(ref) ? cfg().get<string>(`${ref}.baseUrl`, '').trim() : (customOf(ref)?.baseUrl ?? '');
}

/** What this window picked: provider, model per provider, engine. Kept per window, not in global settings. */
export interface WindowChoice {
  provider?: string;
  engine?: string;
  models?: Record<string, string>;
  claudeModel?: string;
  /** Night mode: the model that writes the plan, and the free model that runs it in a loop. */
  nightPlanner?: NightModel;
  nightWorker?: NightModel;
  /** The model that refines prompts, and the cheap helper for file summaries and research subagents. */
  refineModel?: NightModel;
  helperModel?: NightModel;
  /** Reasoning effort for every model; unset is each model's default. */
  effort?: Effort;
  /** The custom chat mode in use (a `.chatmode.md` name); unset is the default mode. */
  chatMode?: string;
  /** Tools and MCP servers turned off for this window's chats (the tools chip). */
  offTools?: string[];
}
export interface NightModel {
  kind: string;
  model: string;
}
const CHOICE_KEY = 'task-x.windowChoice';
let choice: WindowChoice = {};
let store: vscode.Memento | undefined;

/** Loads this window's choice from its workspace state. Settings stay the defaults for a new window. */
export function initChoice(memento: vscode.Memento): void {
  store = memento;
  choice = memento.get<WindowChoice>(CHOICE_KEY, {});
}

export async function setWindowChoice(patch: WindowChoice): Promise<void> {
  choice = { ...choice, ...patch };
  await store?.update(CHOICE_KEY, choice);
}

export const chosenEngine = (): string | undefined => choice.engine;
export const windowChoice = (): Readonly<WindowChoice> => choice;
export const chosenClaudeModel = (): string | undefined => choice.claudeModel;
export const nightModels = (): { planner?: NightModel; worker?: NightModel } => ({ planner: choice.nightPlanner, worker: choice.nightWorker });
export const toolModels = (): { refine?: NightModel; helper?: NightModel } => ({ refine: choice.refineModel, helper: choice.helperModel });
/** `task-x.sandbox.enabled`: commands run in the OS sandbox (default on). */
export const sandboxOn = (): boolean => cfg().get<boolean>('sandbox.enabled', true) !== false;
export async function setSandbox(on: boolean): Promise<void> {
  await cfg().update('sandbox.enabled', on, vscode.ConfigurationTarget.Global);
}
export const effort = (): Effort | undefined => (EFFORTS.includes(choice.effort as Effort) ? choice.effort : undefined);

async function updateCustom(fn: (list: CustomProvider[]) => CustomProvider[]): Promise<void> {
  await cfg().update('customProviders', fn(customProviders()), vscode.ConfigurationTarget.Global);
}

/** Saves a provider. `name` set on a built-in ref, or `ref` empty, adds a new named endpoint. Returns its ref. */
export async function saveProvider(ref: ProviderRef | undefined, kind: ProviderKind, baseUrl: string, model: string, name?: string): Promise<ProviderRef> {
  if (ref && isProviderKind(ref) && !name) {
    await cfg().update(`${ref}.baseUrl`, baseUrl, vscode.ConfigurationTarget.Global);
    await setProvider(ref, model);
    return ref;
  }
  const existing = ref ? customOf(ref) : undefined;
  const id = existing?.id ?? randomBytes(4).toString('hex');
  const entry: CustomProvider = { id, name: (name || existing?.name || refLabel(kind)).slice(0, MAX_NAME), kind, baseUrl, model };
  await updateCustom((list) => (existing ? list.map((c) => (c.id === id ? entry : c)) : [...list, entry]));
  await setWindowChoice({ provider: CUSTOM + id, models: { ...choice.models, [CUSTOM + id]: model } });
  return CUSTOM + id;
}

export async function removeProvider(secrets: vscode.SecretStorage, ref: ProviderRef): Promise<void> {
  const c = customOf(ref);
  if (!c) return;
  await updateCustom((list) => list.filter((x) => x.id !== c.id));
  await secrets.delete(secretKey(ref));
  if (activeRef() === ref) await setWindowChoice({ provider: c.kind });
}

function cfg() {
  return vscode.workspace.getConfiguration(SECTION);
}

/** How much the agent may do without asking: everything (auto), the project only (safe), or nothing (ask). */
export type { EditMode } from './protocol.js';
export const EDIT_MODES: readonly EditMode[] = ['auto', 'safe', 'ask'];

/** `task-x.agent.editMode`; before it was set, `agent.approveAll: false` meant reviewing calls (safe). */
export function editMode(): EditMode {
  const set = cfg().inspect<string>('agent.editMode');
  const v = set?.workspaceFolderValue ?? set?.workspaceValue ?? set?.globalValue;
  if (EDIT_MODES.includes(v as EditMode)) return v as EditMode;
  return cfg().get<boolean>('agent.approveAll', true) ? 'auto' : 'safe';
}

export async function setEditMode(m: EditMode): Promise<void> {
  await cfg().update('agent.editMode', m, vscode.ConfigurationTarget.Global);
}

export function activeRef(): ProviderRef {
  const k = choice.provider ?? cfg().get<string>('provider');
  return isProviderRef(k) ? k : 'ollama';
}

export function activeKind(): ProviderKind {
  return refKind(activeRef());
}

export function modelFor(ref: ProviderRef): string {
  const chosen = choice.models?.[ref];
  if (chosen !== undefined) return chosen.trim();
  return isProviderKind(ref) ? cfg().get<string>(`${ref}.model`, '').trim() : (customOf(ref)?.model ?? '');
}

export function settings() {
  const c = cfg();
  const num = (key: string, def: number) => {
    const v = c.get<number>(key, def);
    return Number.isFinite(v) && v > 0 ? Math.floor(v) : def;
  };
  return {
    timeoutMs: num('requestTimeoutMs', DEFAULT_TIMEOUT_MS),
    maxTokens: num('maxTokens', DEFAULT_MAX_TOKENS),
    systemPrompt: c.get<string>('systemPrompt', '').trim(),
    runtimePath: c.get<string>('runtimePath', ''),
    cdpEndpoint: c.get<string>('cdpEndpoint', '')
  };
}

/** `fallbackModel` stands in when no model is set yet (for listing models). */
export async function providerConfig(secrets: vscode.SecretStorage, ref: ProviderRef = activeRef(), fallbackModel?: string): Promise<ProviderConfig> {
  const model = modelFor(ref) || fallbackModel;
  if (!model) throw new Error(`No model configured for ${refLabel(ref)}. Run "task-x: Select Provider and Model".`);
  return providerConfigWith(secrets, ref, model);
}

/** A provider's endpoint and key with an explicit model (Night mode picks its own models). */
export async function providerConfigWith(secrets: vscode.SecretStorage, ref: ProviderRef, model: string): Promise<ProviderConfig> {
  const apiKey = (await secrets.get(secretKey(ref))) || undefined;
  return { kind: refKind(ref), model, baseUrl: baseUrlFor(ref) || undefined, apiKey, timeoutMs: settings().timeoutMs };
}

export async function setProvider(ref: ProviderRef, model: string): Promise<void> {
  await setWindowChoice({ provider: ref, models: { ...choice.models, [ref]: model } });
}

export const storeApiKey = (s: vscode.SecretStorage, ref: ProviderRef, key: string) => s.store(secretKey(ref), key);
export const deleteApiKey = (s: vscode.SecretStorage, ref: ProviderRef) => s.delete(secretKey(ref));
export const getApiKey = async (s: vscode.SecretStorage, ref: ProviderRef) => (await s.get(secretKey(ref))) || undefined;
export const hasApiKey = async (s: vscode.SecretStorage, ref: ProviderRef) => Boolean(await s.get(secretKey(ref)));

/** Settings moved from `task-x.*` to `task-x.*` when the extension was renamed. */
const OLD_SECTION = 'anvil';

/**
 * Copies settings saved under the old `task-x.` prefix to `task-x.` (user and workspace), unless the
 * new key is already set there, and removes the old keys. `engine: "anvil"` becomes `"task-x"`.
 */
export async function migrateSettings(keys: string[]): Promise<number> {
  const root = vscode.workspace.getConfiguration();
  const targets = [
    [vscode.ConfigurationTarget.Global, 'globalValue'],
    [vscode.ConfigurationTarget.Workspace, 'workspaceValue'],
  ] as const;
  let moved = 0;
  for (const key of keys) {
    const old = root.inspect(`${OLD_SECTION}.${key}`);
    const cur = root.inspect(`${SECTION}.${key}`);
    for (const [target, field] of targets) {
      if (target === vscode.ConfigurationTarget.Workspace && !vscode.workspace.workspaceFolders?.length) continue;
      let value = old?.[field];
      if (value === undefined) continue;
      if (key === 'engine' && value === OLD_SECTION) value = SECTION;
      try {
        if (cur?.[field] === undefined) await root.update(`${SECTION}.${key}`, value, target);
        await root.update(`${OLD_SECTION}.${key}`, undefined, target);
        moved++;
      } catch {
        // An unwritable settings file leaves the old value in place; the new default applies.
      }
    }
  }
  return moved;
}
