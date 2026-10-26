import type { Readable, Writable } from 'node:stream';
import { ErrorCode } from './protocol.js';

const MAX_LINE_CHARS = 32 * 1024 * 1024;

export class RpcError extends Error {
  constructor(
    readonly code: number,
    message: string,
    readonly data?: unknown,
  ) {
    super(message);
    this.name = 'RpcError';
  }

  get cancelled(): boolean {
    return this.code === ErrorCode.RequestCancelled;
  }
}

export interface RequestOptions {
  signal?: AbortSignal;
  timeoutMs?: number;
}

type Handler = (params: unknown) => void;
type RequestHandler = (params: unknown) => Promise<unknown>;

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  cleanup: () => void;
}

/** JSON-RPC 2.0 client over newline-delimited JSON streams. */
export class JsonRpcClient {
  private nextId = 1;
  private buf = '';
  private closed: Error | undefined;
  private readonly pending = new Map<number, Pending>();
  private readonly handlers = new Map<string, Set<Handler>>();
  private readonly requestHandlers = new Map<string, RequestHandler>();

  constructor(
    input: Readable,
    private readonly output: Writable,
    private readonly onProtocolError: (msg: string) => void = () => {},
  ) {
    input.setEncoding('utf8');
    input.on('data', (chunk: string) => this.onData(chunk));
    input.on('end', () => this.dispose(new Error('runtime closed its output')));
    input.on('error', (e) => this.dispose(e));
    output.on('error', (e) => this.dispose(e));
  }

  /** Starts a request and returns its id synchronously so callers can correlate notifications. */
  start<T>(method: string, params?: unknown, opts: RequestOptions = {}): { id: number; result: Promise<T> } {
    const id = this.nextId++;
    const result = new Promise<T>((resolve, reject) => {
      if (this.closed) return reject(this.closed);
      if (opts.signal?.aborted) return reject(new RpcError(ErrorCode.RequestCancelled, 'request cancelled'));
      const cancel = () => this.notify('$/cancelRequest', { id });
      const timer = opts.timeoutMs
        ? setTimeout(() => {
            cancel();
            this.settle(id, undefined, new RpcError(ErrorCode.RequestCancelled, `request timed out after ${opts.timeoutMs}ms`));
          }, opts.timeoutMs)
        : undefined;
      opts.signal?.addEventListener('abort', cancel, { once: true });
      this.pending.set(id, {
        resolve: resolve as (v: unknown) => void,
        reject,
        cleanup: () => {
          clearTimeout(timer);
          opts.signal?.removeEventListener('abort', cancel);
        },
      });
      this.write({ jsonrpc: '2.0', id, method, params });
    });
    return { id, result };
  }

  request<T>(method: string, params?: unknown, opts?: RequestOptions): Promise<T> {
    return this.start<T>(method, params, opts).result;
  }

  notify(method: string, params?: unknown): void {
    if (!this.closed) this.write({ jsonrpc: '2.0', method, params });
  }

  onNotification(method: string, handler: Handler): () => void {
    const set = this.handlers.get(method) ?? new Set();
    set.add(handler);
    this.handlers.set(method, set);
    return () => set.delete(handler);
  }

  /** Handles requests the runtime sends to the client (for example approval prompts). */
  onRequest(method: string, handler: RequestHandler): () => void {
    this.requestHandlers.set(method, handler);
    return () => this.requestHandlers.delete(method);
  }

  get isClosed(): boolean {
    return this.closed !== undefined;
  }

  dispose(reason: Error = new Error('client disposed')): void {
    if (this.closed) return;
    this.closed = reason;
    for (const id of [...this.pending.keys()]) this.settle(id, undefined, reason);
    this.handlers.clear();
    this.requestHandlers.clear();
  }

  private write(msg: object): void {
    this.output.write(JSON.stringify(msg) + '\n');
  }

  private settle(id: number, value: unknown, err?: Error): void {
    const p = this.pending.get(id);
    if (!p) return;
    this.pending.delete(id);
    p.cleanup();
    if (err) p.reject(err);
    else p.resolve(value);
  }

  private onData(chunk: string): void {
    this.buf += chunk;
    let nl: number;
    while ((nl = this.buf.indexOf('\n')) >= 0) {
      const line = this.buf.slice(0, nl).trim();
      this.buf = this.buf.slice(nl + 1);
      if (line) this.onLine(line);
    }
    if (this.buf.length > MAX_LINE_CHARS) {
      this.buf = '';
      this.onProtocolError('incoming message exceeded size limit');
    }
  }

  private async answer(id: unknown, method: string, params: unknown): Promise<void> {
    const h = this.requestHandlers.get(method);
    if (!h) {
      if (!this.closed) this.write({ jsonrpc: '2.0', id, error: { code: ErrorCode.MethodNotFound, message: `no handler for ${method}` } });
      return;
    }
    try {
      const result = await h(params);
      if (!this.closed) this.write({ jsonrpc: '2.0', id, result: result ?? null });
    } catch (e) {
      if (!this.closed) this.write({ jsonrpc: '2.0', id, error: { code: ErrorCode.Internal, message: (e as Error).message } });
    }
  }

  private onLine(line: string): void {
    let msg: { id?: unknown; method?: unknown; params?: unknown; result?: unknown; error?: { code: number; message: string; data?: unknown } };
    try {
      msg = JSON.parse(line);
    } catch {
      this.onProtocolError('runtime sent invalid JSON');
      return;
    }
    if (typeof msg.method === 'string' && msg.id !== undefined) {
      void this.answer(msg.id, msg.method, msg.params);
      return;
    }
    if (typeof msg.method === 'string' && msg.id === undefined) {
      for (const h of this.handlers.get(msg.method) ?? []) {
        try {
          h(msg.params);
        } catch (e) {
          this.onProtocolError(`notification handler failed: ${(e as Error).message}`);
        }
      }
      return;
    }
    if (typeof msg.id !== 'number') {
      if (msg.error) this.onProtocolError(`runtime error: ${msg.error.message}`);
      return;
    }
    if (msg.error) this.settle(msg.id, undefined, new RpcError(msg.error.code, msg.error.message, msg.error.data));
    else this.settle(msg.id, msg.result);
  }
}
