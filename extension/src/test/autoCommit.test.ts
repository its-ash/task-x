import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { commitPaths, commitSubject } from '../autoCommit.js';

const git = (cwd: string, ...args: string[]) => execFileSync('git', args, { cwd, encoding: 'utf8' });
const repo = () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'task-x-commit-'));
  git(dir, 'init', '-q');
  git(dir, 'config', 'user.name', 'test');
  git(dir, 'config', 'user.email', 'test@example.com');
  return dir;
};

test('subject uses the first line of the request and stays short', () => {
  assert.equal(commitSubject('Add a login page\nwith tests'), 'task-x: Add a login page');
  assert.equal(commitSubject('   '), 'task-x: agent changes');
  assert.equal(commitSubject('x'.repeat(200)).length, 72);
});

test('commits only the files the task changed', async () => {
  const dir = repo();
  fs.writeFileSync(path.join(dir, 'task.txt'), 'task\n');
  fs.writeFileSync(path.join(dir, 'mine.txt'), 'my work in progress\n');
  git(dir, 'add', 'mine.txt');

  const r = await commitPaths(dir, ['task.txt'], 'task-x: test');
  assert.deepEqual(r, { committed: true, subject: 'task-x: test' });
  assert.equal(git(dir, 'log', '-1', '--format=%s').trim(), 'task-x: test');
  assert.deepEqual(git(dir, 'show', '--name-only', '--format=', 'HEAD').trim().split('\n'), ['task.txt']);
  assert.match(git(dir, 'status', '--porcelain'), /A  mine\.txt/);
});

test('reports nothing to commit and non-git folders', async () => {
  const dir = repo();
  fs.writeFileSync(path.join(dir, 'a.txt'), 'a\n');
  await commitPaths(dir, ['a.txt'], 'task-x: first');
  assert.deepEqual(await commitPaths(dir, ['a.txt'], 'task-x: again'), { committed: false, reason: 'the files match the last commit' });
  assert.deepEqual(await commitPaths(dir, [], 'task-x: none'), { committed: false, reason: 'no files changed' });

  const plain = fs.mkdtempSync(path.join(os.tmpdir(), 'task-x-plain-'));
  fs.writeFileSync(path.join(plain, 'b.txt'), 'b\n');
  assert.deepEqual(await commitPaths(plain, ['b.txt'], 'task-x: b'), { committed: false, reason: 'the workspace is not a git repository' });
});
