import assert from 'node:assert/strict';
import { test } from 'node:test';
import { formatTokens, listModels, missingNote, parseOllamaList, withSaved } from '../models.js';
import type { ProviderConfig } from '../protocol.js';

const ollama: ProviderConfig = { kind: 'ollama', model: 'probe' };

test('reads model names from ollama list', () => {
  const out = 'NAME              ID              SIZE      MODIFIED\nqwen3:8b          500a1f067a9f    5.2 GB    2 days ago\nllama3.2:latest   a80c4f17acd5    2.0 GB    3 weeks ago\n\n';
  assert.deepEqual(parseOllamaList(out), ['qwen3:8b', 'llama3.2:latest']);
  assert.deepEqual(parseOllamaList('NAME ID SIZE MODIFIED\n'), []);
});

test('falls back to the Ollama CLI when the server does not answer', async () => {
  const down = async () => Promise.reject(new Error('connection refused'));
  const r = await listModels(down, ollama, async () => ['qwen3:8b']);
  assert.deepEqual(r.models, [{ id: 'qwen3:8b', capabilities: [] }]);
  assert.match(r.note ?? '', /ollama serve/);
  assert.equal(r.error, 'connection refused');
  const empty = await listModels(down, ollama, async () => []);
  assert.match(empty.note ?? '', /ollama pull/);
  const missing = await listModels(down, ollama, async () => undefined);
  assert.deepEqual([missing.models, missing.note], [[], undefined], 'no CLI: only the error');
  const cloud = await listModels(down, { kind: 'openai', model: 'm' }, async () => ['never']);
  assert.deepEqual(cloud.models, [], 'only Ollama falls back to the CLI');
  const up = await listModels(async () => [{ id: 'a', contextWindow: 8192, capabilities: ['tools'] }], ollama);
  assert.deepEqual(up, { models: [{ id: 'a', contextWindow: 8192, capabilities: ['tools'] }] });
});

test('formats context sizes', () => {
  assert.deepEqual([131072, 200_000, 40960, 1_000_000, 1_048_576, 0, undefined].map(formatTokens), ['128K', '200K', '40K', '1M', '1M', '', '']);
});

test('shows a saved model only while the list is unknown', () => {
  const installed = [{ id: 'llama3.2:latest', capabilities: [] }];
  assert.deepEqual(withSaved(installed, 'qwen3').map((m) => m.id), ['qwen3', 'llama3.2:latest'], 'not listed yet: still pickable');
  assert.deepEqual(withSaved(installed, 'qwen3', true).map((m) => m.id), ['llama3.2:latest'], 'a complete list never shows a model that is not installed');
  assert.equal(withSaved(installed, 'llama3.2').length, 1, 'llama3.2 and llama3.2:latest are the same model');
  assert.equal(withSaved([], '').length, 0);
  assert.match(missingNote(installed, 'qwen3', true) ?? '', /ollama pull qwen3/);
  assert.equal(missingNote(installed, 'llama3.2', true), undefined);
  assert.match(missingNote(installed, 'gpt-x', false) ?? '', /not in this endpoint/);
});
