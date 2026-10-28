import * as vscode from 'vscode';
import { editMode, isProviderRef, providerConfig, settings, SECTION } from './config.js';
import { type ChatMode, type EditorContext, type ImageInput, type PolicyConfig, type PriceRule, type ProviderConfig, ROLES, type Role } from './protocol.js';

const TASK_KINDS = ['question', 'bugInvestigation', 'codeModification', 'newFeature', 'refactoring', 'testing', 'documentation', 'repositoryWide', 'architecture'];
const MAX_PRICE_RULES = 100;

const cfg = () => vscode.workspace.getConfiguration(SECTION);

/** Provider configs for the given refs; refs without a model or key are left out. */
export async function configs(secrets: vscode.SecretStorage, refs: unknown[]): Promise<ProviderConfig[]> {
  const all = await Promise.all(refs.filter(isProviderRef).map((k) => providerConfig(secrets, k).catch(() => undefined)));
  return all.filter((x): x is ProviderConfig => x !== undefined);
}

/**
 * The edit mode decides what asks: auto runs everything, safe asks only before what can reach beyond
 * the workspace, ask asks before every write and command. Commands run in the OS sandbox; tools in
 * `agent.allowTools` never ask, tools in `agent.denyTools` never run.
 */
export function policy(): PolicyConfig {
  const strings = (k: string) => cfg().get<string[]>(k, []).filter((x) => typeof x === 'string');
  const em = editMode();
  // allowAll keeps MCP tools offered in read-only chat modes when nothing asks.
  return { mode: 'autonomous', allow: strings('agent.allowTools'), deny: strings('agent.denyTools'), allowAll: em === 'auto', editMode: em };
}

export function limits() {
  const c = cfg();
  const clamp = (v: unknown, lo: number, hi: number, def: number) => (typeof v === 'number' && Number.isFinite(v) ? Math.min(hi, Math.max(lo, Math.floor(v))) : def);
  return {
    maxIterations: clamp(c.get('agent.maxIterations'), 1, 500, 100),
    maxOutputTokens: settings().maxTokens,
    maxParallel: clamp(c.get('team.maxParallel'), 1, 8, 3),
    // 0 lets the runtime use the model's known window.
    contextWindowTokens: clamp(c.get('context.windowTokens'), 0, 10_000_000, 0) || undefined,
    compactPercent: clamp(c.get('context.compactAt'), 30, 95, 85),
  };
}

/** `task-x.pricing` rules that are well formed. */
export function prices(): PriceRule[] {
  const ok = (n: unknown): n is number => typeof n === 'number' && Number.isFinite(n) && n >= 0;
  return cfg()
    .get<unknown[]>('pricing', [])
    .filter((r): r is PriceRule => {
      const x = r as PriceRule;
      return typeof x?.model === 'string' && !!x.model.trim() && ok(x.inputPerMtok) && ok(x.outputPerMtok);
    })
    .slice(0, MAX_PRICE_RULES)
    .map((r) => ({ model: r.model.trim(), inputPerMtok: r.inputPerMtok, outputPerMtok: r.outputPerMtok }));
}

/** `task-x.routing`: a provider for each task kind; entries without a usable provider are left out. */
export async function routes(secrets: vscode.SecretStorage): Promise<{ kinds: string[]; provider: ProviderConfig }[]> {
  const routing = cfg()
    .get<{ kinds: string[]; provider: string }[]>('routing', [])
    .filter((r) => Array.isArray(r?.kinds) && isProviderRef(r.provider))
    .map((r) => ({ ...r, kinds: r.kinds.filter((k) => TASK_KINDS.includes(k)) }))
    .filter((r) => r.kinds.length);
  const all = await Promise.all(routing.map(async (r) => ({ kinds: r.kinds, provider: (await configs(secrets, [r.provider]))[0] })));
  return all.filter((r): r is { kinds: string[]; provider: ProviderConfig } => !!r.provider);
}

/** Full `agent/run` params: the active provider plus routes, fallbacks, role models, prices, policy, and limits. */
export async function runParams(secrets: vscode.SecretStorage, o: { prompt: string; mode: ChatMode; taskId: string; editor: EditorContext; provider?: ProviderConfig; images?: ImageInput[] }) {
  const c = cfg();
  const active = c.get<string>('provider');
  const provider = o.provider ?? (await providerConfig(secrets));
  const fallbacks = await configs(secrets, c.get<string[]>('fallbackProviders', []).filter((k) => k !== active));
  const roleMap = c.get<Record<string, string>>('team.roleProviders', {}) ?? {};
  const roleProviders = (
    await Promise.all(
      Object.entries(roleMap)
        .filter(([role, kind]) => ROLES.includes(role as Role) && isProviderRef(kind))
        .map(async ([role, kind]) => ({ role, provider: (await configs(secrets, [kind]))[0] })),
    )
  ).filter((r) => r.provider);
  return {
    prompt: o.prompt,
    images: o.images ?? [],
    provider,
    chatMode: o.mode,
    taskId: o.taskId,
    editor: o.editor,
    policy: policy(),
    routes: await routes(secrets),
    fallbacks,
    roleProviders,
    prices: prices(),
    limits: limits(),
  };
}

/** "$0.0123" or "$1.20"; empty for zero or unknown. */
export function formatCost(usd: number | undefined): string {
  if (typeof usd !== 'number' || !(usd > 0)) return '';
  return `$${usd < 0.01 ? usd.toFixed(4) : usd.toFixed(2)}`;
}
