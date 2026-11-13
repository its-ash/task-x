import { DEFAULT_BASE_URLS, type ProviderKind, requiresApiKey } from './protocol.js';

export interface Draft {
  kind: ProviderKind;
  baseUrl: string;
  model: string;
  apiKey: string;
  /** Display name; required for a named endpoint. */
  name?: string;
  named?: boolean;
}

export type Errors = Partial<Record<keyof Draft, string>>;

const isLoopback = (host: string) => ['localhost', '127.0.0.1', '[::1]'].includes(host.toLowerCase()) || /^127\./.test(host);

/** Mirrors the runtime's `ProviderConfig::resolve` checks so errors show next to the field. */
export function validateDraft(d: Draft, storedKey: boolean, needModel: boolean): Errors {
  const e: Errors = {};
  if (d.named && !d.name?.trim()) e.name = 'Enter a name for this endpoint.';
  else if (d.name && d.name.length > 60) e.name = 'Use 60 characters or fewer.';
  if (d.baseUrl) {
    try {
      const u = new URL(d.baseUrl);
      if (u.protocol !== 'http:' && u.protocol !== 'https:') e.baseUrl = 'Use an http or https URL.';
      else if (u.username || u.password) e.baseUrl = 'Do not put credentials in the URL. Use the API key field.';
      else if (u.protocol === 'http:' && (d.apiKey || storedKey) && requiresApiKey(d.kind) && !isLoopback(u.hostname))
        e.baseUrl = 'An API key can only be sent over http to localhost. Use https.';
    } catch {
      e.baseUrl = 'Enter a valid URL.';
    }
  } else if (!DEFAULT_BASE_URLS[d.kind]) {
    e.baseUrl = 'Base URL is required for an OpenAI-compatible endpoint.';
  }
  if (/\s/.test(d.apiKey)) e.apiKey = 'The API key must not contain spaces.';
  else if (requiresApiKey(d.kind) && !d.apiKey && !storedKey) e.apiKey = 'An API key is required for this provider.';
  if (needModel && !d.model) e.model = 'Enter or select a model.';
  else if (/[\u0000-\u001f]/.test(d.model) || d.model.length > 256) e.model = 'Enter a valid model id.';
  return e;
}

