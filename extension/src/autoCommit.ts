// Commits the files one task changed. Free of vscode imports so it can be tested with a real git repository.
import { execFile } from 'node:child_process';

const MAX_SUBJECT = 72;

/** Commit subject for a task: `task-x:` and the first line of the request, shortened. */
export function commitSubject(prompt: string): string {
  const line = prompt.split('\n')[0].trim();
  const short = line.length > MAX_SUBJECT - 'task-x: '.length ? `${line.slice(0, MAX_SUBJECT - 'task-x: '.length - 1)}…` : line;
  return `task-x: ${short || 'agent changes'}`;
}

const git = (cwd: string, args: string[]) =>
  new Promise<string>((resolve, reject) =>
    execFile('git', ['--literal-pathspecs', ...args], { cwd, timeout: 30_000, maxBuffer: 1 << 20 }, (e, out, err) =>
      e ? reject(new Error(String(err || e.message).trim())) : resolve(out),
    ),
  );

export type CommitResult = { committed: true; subject: string } | { committed: false; reason: string };

/** Stages and commits only `paths`; other staged or unstaged work in the repository is left alone. */
export async function commitPaths(root: string, paths: string[], subject: string): Promise<CommitResult> {
  if (!paths.length) return { committed: false, reason: 'no files changed' };
  try {
    await git(root, ['rev-parse', '--is-inside-work-tree']);
  } catch {
    return { committed: false, reason: 'the workspace is not a git repository' };
  }
  await git(root, ['add', '-A', '--', ...paths]);
  const staged = await git(root, ['diff', '--cached', '--name-only', '--', ...paths]);
  if (!staged.trim()) return { committed: false, reason: 'the files match the last commit' };
  await git(root, ['commit', '-m', subject, '--', ...paths]);
  return { committed: true, subject };
}
