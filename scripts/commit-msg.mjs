import { execFileSync } from 'node:child_process';

const BASE = process.env.COMMIT_LLM_URL ?? 'http://127.0.0.1:9694/v1';
const MODEL = process.env.COMMIT_MODEL ?? '';
const MAX_DIFF = 60_000;
const TIMEOUT_MS = 120_000;
const PROMPT =
  'Analyze the staged git changes and generate a concise commit message. Output ONLY the commit message. Do not include quotes, markdown, explanation, or bullet points.';

const req = async (path, body) => {
  const res = await fetch(`${BASE}${path}`, {
    method: body ? 'POST' : 'GET',
    headers: body ? { 'content-type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(TIMEOUT_MS),
  });
  if (!res.ok) throw new Error(`${path}: HTTP ${res.status} ${(await res.text()).slice(0, 500)}`);
  return res.json();
};

const clean = (s) =>
  s
    .replace(/<think>[\s\S]*?<\/think>/gi, '')
    .replace(/^```\w*\n?|```$/gm, '')
    .trim()
    .replace(/^["'`]+|["'`]+$/g, '')
    .trim();

try {
  const diff = execFileSync('git', ['diff', '--cached', '--stat', '--patch'], { encoding: 'utf8', maxBuffer: 1 << 28 });
  if (!diff.trim()) throw new Error('no staged changes');
  const model = MODEL || (await req('/models')).data?.[0]?.id;
  if (!model) throw new Error(`no model available at ${BASE}`);
  const res = await req('/chat/completions', {
    model,
    temperature: 0.2,
    messages: [
      { role: 'system', content: PROMPT },
      { role: 'user', content: diff.length > MAX_DIFF ? `${diff.slice(0, MAX_DIFF)}\n[diff truncated]` : diff },
    ],
  });
  const msg = clean(res.choices?.[0]?.message?.content ?? '');
  if (!msg) throw new Error('empty commit message');
  process.stdout.write(msg);
} catch (e) {
  console.error(`commit-msg: ${e instanceof Error ? e.message : e}`);
  process.exit(1);
}
