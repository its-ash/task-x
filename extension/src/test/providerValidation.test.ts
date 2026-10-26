import assert from 'node:assert/strict';
import { test } from 'node:test';
import { type Draft, validateDraft } from '../providerValidation.js';

const d = (o: Partial<Draft>): Draft => ({ kind: 'openaiCompatible', baseUrl: 'https://llm.example.com/v1', model: 'm', apiKey: 'k', ...o });

test('OpenAI-compatible needs a base URL and an API key', () => {
  assert.deepEqual(validateDraft(d({ baseUrl: '', apiKey: '' }), false, true), {
    baseUrl: 'Base URL is required for an OpenAI-compatible endpoint.',
    apiKey: 'An API key is required for this provider.',
  });
  assert.deepEqual(validateDraft(d({ apiKey: '' }), true, true), {});
});

test('Ollama key is optional and default URL applies', () => {
  assert.deepEqual(validateDraft(d({ kind: 'ollama', baseUrl: '', apiKey: '' }), false, true), {});
});

test('rejects unsafe URLs', () => {
  assert.match(validateDraft(d({ baseUrl: 'ftp://x' }), false, true).baseUrl!, /http or https/);
  assert.match(validateDraft(d({ baseUrl: 'https://u:p@x/v1' }), false, true).baseUrl!, /credentials/);
  assert.match(validateDraft(d({ baseUrl: 'http://remote.example/v1' }), false, true).baseUrl!, /localhost/);
  assert.deepEqual(validateDraft(d({ baseUrl: 'http://127.0.0.1:8080/v1' }), false, true), {});
  assert.match(validateDraft(d({ baseUrl: 'not a url' }), false, true).baseUrl!, /valid URL/);
});

test('model is required only when saving or testing', () => {
  assert.equal(validateDraft(d({ model: '' }), false, true).model, 'Enter or select a model.');
  assert.deepEqual(validateDraft(d({ model: '' }), false, false), {});
  assert.match(validateDraft(d({ apiKey: 'a b' }), false, true).apiKey!, /spaces/);
});
