import { type ChildProcess, spawn } from 'node:child_process';
import { accessSync, constants, statSync } from 'node:fs';
import path from 'node:path';
import { type InitializeResult, PROTOCOL_VERSION } from './protocol.js';
import { JsonRpcClient } from './rpc.js';

const BINARY = process.platform === 'win32' ? 'task-x-runtime.exe' : 'task-x-runtime';
const INIT_TIMEOUT_MS = 10_000;
const SHUTDOWN_TIMEOUT_MS = 3_000;
// Only these variables reach the runtime; it needs network/proxy/locale settings, not the full host env.
// USER and LOGNAME let the claude CLI find its login in the macOS Keychain.
export const ENV_ALLOWLIST = ['PATH', 'HOME', 'USER', 'LOGNAME', 'CLAUDE_CONFIG_DIR', 'XDG_CONFIG_HOME', 'USERPROFILE', 'SYSTEMROOT', 'TMPDIR', 'TEMP', 'TMP', 'LANG', 'LC_ALL', 'HTTPS_PROXY', 'HTTP_PROXY', 'NO_PROXY', 'https_proxy', 'http_proxy', 'no_proxy', 'SSL_CERT_FILE', 'SSL_CERT_DIR', 'ANVIL_LOG'];

export const runtimeEnv = (): NodeJS.ProcessEnv => Object.fromEntries(ENV_ALLOWLIST.flatMap((k) => (process.env[k] !== undefined ? [[k, process.env[k]!]] : [])));

function isExecutable(p: string): boolean {
  try {
    if (!statSync(p).isFile()) return false;
    accessSync(p, process.platform === 'win32' ? constants.F_OK : constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/** Resolution order: explicit setting, bundled `bin/`, then a local cargo build (release before debug). */
export function findRuntime(extensionPath: string, configured?: string): string {
  if (configured?.trim()) {
    const p = configured.trim();
    if (!path.isAbsolute(p)) throw new Error('task-x.runtimePath must be an absolute path');
    if (!isExecutable(p)) throw new Error(`task-x.runtimePath is not an executable file: ${p}`);
    return p;
  }
  const target = path.resolve(extensionPath, '..', 'runtime', 'target');
  const candidates = [path.join(extensionPath, 'bin', BINARY), path.join(target, 'release', BINARY), path.join(target, 'debug', BINARY)];
  const found = candidates.find(isExecutable);
  if (!found) throw new Error(`task-x-runtime not found. Build it with "make build" or set task-x.runtimePath. Looked in:\n${candidates.join('\n')}`);
  return found;
}

export class RuntimeProcess {
  readonly client: JsonRpcClient;
  private exited = false;

  private constructor(
    private readonly child: ChildProcess,
    onExit: (code: number | null, signal: NodeJS.Signals | null) => void,
    log: (line: string) => void,
  ) {
    this.client = new JsonRpcClient(child.stdout!, child.stdin!, (m) => log(`protocol: ${m}`));
    child.stderr!.setEncoding('utf8');
    child.stderr!.on('data', (d: string) => d.split('\n').filter(Boolean).forEach(log));
    child.on('exit', (code, signal) => {
      this.exited = true;
      this.client.dispose(new Error(`runtime exited (${signal ?? code})`));
      onExit(code, signal);
    });
  }

  static async start(
    binary: string,
    opts: { clientVersion: string; dataDir?: string; sandbox?: boolean; log: (line: string) => void; onExit: (code: number | null, signal: NodeJS.Signals | null) => void },
  ): Promise<{ runtime: RuntimeProcess; info: InitializeResult }> {
    const env = runtimeEnv();
    const child = spawn(binary, [], { stdio: ['pipe', 'pipe', 'pipe'], env, windowsHide: true });
    await new Promise<void>((resolve, reject) => {
      child.once('spawn', resolve);
      child.once('error', reject);
    });
    const runtime = new RuntimeProcess(child, opts.onExit, opts.log);
    try {
      const info = await runtime.client.request<InitializeResult>(
        'initialize',
        { protocolVersion: PROTOCOL_VERSION, clientInfo: { name: 'task-x-vscode', version: opts.clientVersion }, dataDir: opts.dataDir, sandbox: opts.sandbox },
        { timeoutMs: INIT_TIMEOUT_MS },
      );
      return { runtime, info };
    } catch (e) {
      runtime.kill();
      throw e;
    }
  }

  get running(): boolean {
    return !this.exited;
  }

  /** Graceful shutdown: `shutdown` cancels in-flight work, `exit` ends the process; kill on timeout. */
  async stop(): Promise<void> {
    if (this.exited) return;
    const exited = new Promise<void>((r) => this.child.once('exit', () => r()));
    const timeout = new Promise<void>((r) => setTimeout(r, SHUTDOWN_TIMEOUT_MS).unref());
    try {
      await Promise.race([this.client.request('shutdown', undefined, { timeoutMs: SHUTDOWN_TIMEOUT_MS }), timeout]);
      this.client.notify('exit');
    } catch {
      // Fall through to the kill below.
    }
    await Promise.race([exited, timeout]);
    this.kill();
  }

  kill(): void {
    if (!this.exited) this.child.kill();
  }
}
