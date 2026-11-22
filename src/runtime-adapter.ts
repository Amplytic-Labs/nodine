/**
 * Structural type of the private Nodine runtime supervisor as consumed by
 * this SDK. Every member matches the real implementation (poc/p2-007/public/
 * supervisor.mjs, Supervisor class); the SDK performs runtime type checks in
 * development builds and adapts, never reimplements.
 *
 * This type is internal: it is not exported from the package index. It
 * exists so the adapter layer is honest about the exact surface it uses.
 */

/** Minimal output sink the supervisor writes terminal output to. */
export interface TermSink {
  /** Called with process output text (stdout/stderr, ANSI included). */
  write(text: string): void;
  /** Called with the terminal's own status messages. */
  meta(text: string): void;
}

/**
 * A headless replacement for the runtime's DOM Term class: collects all
 * output into a string buffer and fans it out to subscribers. The supervisor
 * only ever calls write() and meta() (verified: supervisor.mjs Term usage).
 */
export class HeadlessTermSink implements TermSink {
  private buffer = '';
  private readonly subscribers = new Set<(chunk: string) => void>();

  write(text: string): void {
    this.buffer += text;
    for (const fn of this.subscribers) {
      try { fn(text); } catch { /* subscriber errors must not break the runtime */ }
    }
  }

  meta(text: string): void {
    this.write(text);
  }

  /** Everything written so far (raw, ANSI sequences included). */
  dump(): string {
    return this.buffer;
  }

  /** Subscribe to future chunks. Returns an unsubscribe function. */
  onTap(fn: (chunk: string) => void): () => void {
    this.subscribers.add(fn);
    return () => { this.subscribers.delete(fn); };
  }
}

/** Internal process record shape returned by Supervisor.spawnProcess. */
export interface RuntimeProcessRecord {
  pid: number;
  worker: Worker;
  port: MessagePort;
  ep: { index: number; generation: number };
  parent: RuntimeProcessRecord | null;
  children: Set<number>;
  stdioMode: string;
  stdioArr?: string[];
  argv: string[];
  entry: string | null;
  env: Record<string, string>;
  cwd: string;
  ready: boolean;
  exited: boolean;
  parentPid: number | null;
}

/** fs helper surface on the supervisor (all kernel-backed). */
export interface SupervisorFsLike {
  kstat(path: string): Promise<{ ok: boolean; status?: number; mode?: number; size?: number; isDir?: boolean; isFile?: boolean }>;
  klstat(path: string): Promise<{ ok: boolean; status?: number; mode?: number; size?: number; isDir?: boolean; isFile?: boolean; isSymlink?: boolean }>;
  mkdirP(dir: string): Promise<void>;
  kmkdirOne(path: string, mode?: number): Promise<number>;
  kunlink(path: string): Promise<number>;
  krmdir(path: string): Promise<number>;
  ksymlink(target: string, linkpath: string): Promise<void>;
  kwriteFile(path: string, u8: Uint8Array, mode?: number): Promise<void>;
  kreadWhole(path: string): Promise<Uint8Array | null>;
  kreaddir(path: string): Promise<string[] | null>;
  resolveSymlinks(path: string): Promise<string>;
  abs(cwd: string, p: string): string;
  normalizePath(p: string): string;
  parentOf(p: string): string;
}

/** Structurally-typed view of the runtime Supervisor the SDK relies on. */
export interface RuntimeSupervisor extends SupervisorFsLike {
  /** Kernel boot: loads kernel-driver.js worker, WASM, shared memory. */
  boot(): Promise<void>;
  /** Seeds the VFS from the distribution manifest (npm files). */
  seed(): Promise<void>;
  /** Spawns one process. parent may be a synthetic record owned by the SDK. */
  spawnProcess(opts: {
    argv: string[];
    env?: Record<string, string>;
    cwd?: string;
    stdioMode?: string | string[];
    parent?: RuntimeProcessRecord | null;
    viaPort?: MessagePort | null;
  }): Promise<RuntimeProcessRecord | null>;
  /** Posts a signal to a process and all its descendants. */
  signalTree(root: RuntimeProcessRecord, sig: string): void;
  /** Full runtime shutdown; releases kernel + workers. */
  shutdown(code?: number): Promise<void>;
  /** Wires the preview service-worker bridge. */
  initPreviewSW(sid: string, swPort: MessagePort): void;
  /** Page hook for preview port open/close events. */
  onPreviewEvent: ((ev: { kind: 'open' | 'close'; port: number; pid: number; argv: string[] }) => void) | null;
  /**
   * M1.5 page hook: preview-error events forwarded by the preview service
   * worker (captured by the injected hook inside preview documents).
   */
  onPreviewMessage: ((m: {
    type: 'preview-error';
    sid: string;
    port: number;
    kind: 'uncaught' | 'rejection' | 'console';
    message: string;
    pathname?: string;
    search?: string;
    hash?: string;
    filename?: string | null;
    line?: number | null;
    column?: number | null;
    stack?: string | null;
  }) => void) | null;
  /** Base environment variables (HOME, PATH, npm config …). */
  baseEnv: Record<string, string>;
  /** True once shutdown() has run. */
  stopped: boolean;
  /** Kernel status text setter (no-op when headless). */
  setKernel(cls: string, text: string): void;
  /** Status element writes (no-op when headless). */
  status: { kernel: { textContent: string }; procs: { textContent: string }; cwd: { textContent: string }; exit: { textContent: string } | null; seed: { textContent: string } };
}

/** Runtime module namespace: `import(runtimeUrl)` → this. */
export interface RuntimeModule {
  Supervisor: new (term: TermSink, statusEls: RuntimeSupervisor['status'], perfOpts?: unknown) => RuntimeSupervisor;
}

/**
 * Loads the runtime supervisor module from `runtimeUrl` and validates the
 * structural contract the SDK depends on. Throws BootFailedError-descriptive
 * errors (message-only; the caller wraps) when the runtime does not match.
 */
export async function loadRuntimeModule(runtimeUrl: string | URL): Promise<RuntimeModule> {
  let mod: RuntimeModule;
  try {
    mod = (await import(/* @vite-ignore */ String(runtimeUrl))) as RuntimeModule;
  } catch (e) {
    throw new Error(
      `Could not load the Nodine runtime module from ${String(runtimeUrl)}: ` +
        (e instanceof Error ? e.message : String(e)),
    );
  }
  if (typeof mod.Supervisor !== 'function') {
    throw new Error(
      `The module at ${String(runtimeUrl)} does not export a Supervisor class; ` +
        'it is not a Nodine runtime bundle.',
    );
  }
  return mod;
}

/**
 * Runtime-surface validation: checks the members the SDK calls at runtime
 * (beyond constructor-time), so a mismatch fails loudly at boot with a
 * precise list instead of a confusing TypeError mid-session.
 */
export function assertRuntimeSurface(sup: unknown): asserts sup is RuntimeSupervisor {
  const s = sup as Partial<RuntimeSupervisor>;
  const missing: string[] = [];
  const need = [
    'boot', 'seed', 'spawnProcess', 'signalTree', 'shutdown', 'initPreviewSW',
    'kstat', 'klstat', 'mkdirP', 'kmkdirOne', 'kunlink', 'krmdir', 'ksymlink',
    'kwriteFile', 'kreadWhole', 'kreaddir', 'resolveSymlinks', 'abs',
    'normalizePath', 'parentOf', 'baseEnv', 'stopped', 'setKernel', 'status',
  ];
  for (const k of need) {
    if (s && (s as Record<string, unknown>)[k] === undefined) missing.push(k);
  }
  if (missing.length > 0) {
    throw new Error(
      'The Nodine runtime module is missing required members: ' +
        missing.join(', ') +
        '. The SDK and runtime versions are incompatible.',
    );
  }
}
