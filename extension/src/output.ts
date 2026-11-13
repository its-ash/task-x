// Formatting of tool results for display; free of vscode imports so it can be unit tested.

const quoteArg = (a: string) => (/^[\w@%+=:,./-]+$/.test(a) ? a : `'${a.replace(/'/g, `'\\''`)}'`);

/** Turns a tool result (JSON text) into readable, terminal-like output. */
export function formatOutput(raw: string): string {
  let v: Record<string, unknown>;
  try {
    v = JSON.parse(raw);
  } catch {
    return raw;
  }
  const r = (v.result && typeof v.result === 'object' && 'argv' in (v.result as object) ? v.result : 'argv' in v ? v : undefined) as
    | { argv: string[]; cwd?: string; stdout?: string; stderr?: string; exitCode?: number | null; timedOut?: boolean; durationMs?: number; sandboxed?: boolean; truncated?: boolean }
    | undefined;
  if (r) {
    const lines = [`$ ${r.argv.map(quoteArg).join(' ')}${r.cwd && r.cwd !== '.' ? `    # in ${r.cwd}` : ''}`];
    if (r.stdout) lines.push(r.stdout.replace(/\s+$/, ''));
    if (r.stderr) lines.push(r.stderr.replace(/\s+$/, ''));
    const meta = [r.timedOut ? 'timed out' : `exit ${r.exitCode ?? 'killed'}`, `${r.durationMs ?? 0} ms`];
    if (r.sandboxed) meta.push('sandboxed');
    if (r.truncated) meta.push('output truncated');
    lines.push(`[${meta.join(', ')}]`);
    return lines.join('\n');
  }
  const e = v.error as { kind?: string; message?: string } | undefined;
  if (e?.message) return `${e.kind ?? 'error'}: ${e.message}`;
  for (const k of ['diff', 'show', 'content']) if (typeof v[k] === 'string' && v[k]) return v[k] as string;
  return JSON.stringify(v, null, 2);
}

/** Added and removed line counts of a unified diff; `---`/`+++` file headers are not counted. */
export function diffStats(diff: string): { added: number; removed: number } {
  const lines = diff.split('\n');
  let added = 0;
  let removed = 0;
  for (let i = 0; i < lines.length; i++) {
    const l = lines[i];
    if (l.startsWith('--- ') && lines[i + 1]?.startsWith('+++ ')) {
      i++;
    } else if (l.startsWith('+')) {
      added++;
    } else if (l.startsWith('-')) {
      removed++;
    }
  }
  return { added, removed };
}
