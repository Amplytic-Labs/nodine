/**
 * The {@link Container} implementation: wraps the real Nodine runtime
 * (Supervisor) without modifying it. Boot loads the runtime module the host
 * provides, spawn taps process IO through a synthetic-parent MessageChannel,
 * fs ops ride the supervisor's kernel helpers, preview port events are
 * bridged through the preview service worker.
 *
 * @module
 */

import type {
  BootOptions,
  ContainerEventName,
  ContainerProcess,
  ErrorListener,
  FileSystemAPI,
  FileSystemTree,
  PortListener,
  PreviewMessage,
  PreviewMessageListener,
  ServerReadyListener,
  SpawnOptions,
  TerminalDimensions,
} from './types/index.js';

import {
  BootFailedError,
  ContainerDestroyedError,
  EnvironmentUnsupportedError,
  FileSystemError,
  NotYetImplementedError,
  SpawnError,
} from './errors/index.js';

import { environmentFailures } from './environment.js';
import { SDK_VERSION } from './version.js';
export { SDK_VERSION };
import {
  HeadlessTermSink,
  assertRuntimeSurface,
  loadRuntimeModule,
  type RuntimeProcessRecord,
  type RuntimeSupervisor,
} from './runtime-adapter.js';
import { createFileSystem } from './fs-adapter.js';

type Unsubscribe = () => void;

/**
 * The default {@link BootOptions.runtimeUrl}: the URL the official Vite
 * plugin (`nodine/vite`) serves the runtime bundle at, and the location
 * `npx nodine-stage public/nodine` stages it to. Booting without options
 * uses this path.
 */
export const DEFAULT_RUNTIME_URL = '/nodine/supervisor.mjs';

interface EventHandlers {
  'server-ready': Set<ServerReadyListener>;
  port: Set<PortListener>;
  error: Set<ErrorListener>;
  'preview-message': Set<PreviewMessageListener>;
}

/**
 * The main class of the Nodine API. Obtain an instance with
 * {@link Container.boot}.
 *
 * @public
 */
export class Container {
  /** The SDK package version. */
  static readonly sdkVersion = SDK_VERSION;

  /** Live instance counter (single-boot runtime: 1 while booted). */
  private static bootedInstance: Container | null = null;

  private constructor(
    private readonly supervisor: RuntimeSupervisor,
    private readonly term: HeadlessTermSink,
    private readonly workdirPath: string,
    private readonly runtimeBase: URL,
  ) {}

  /**
   * Boots a Nodine runtime instance.
   *
   * @param options - Boot configuration. `runtimeUrl` optionally points at
   * the `supervisor.mjs` module of the Nodine runtime bundle; when omitted,
   * {@link DEFAULT_RUNTIME_URL} is used — the URL the official Vite plugin
   * (`nodine/vite`) and `npx nodine-stage` serve. The runtime resolves all
   * of its assets relative to that URL, so the bundle can live in any
   * directory the host serves; for preview service-worker registration it
   * should sit at the origin root, or the server must send a
   * `Service-Worker-Allowed: /` header for `sw-nodine.js` (see the
   * README).
   *
   * @throws {@link EnvironmentUnsupportedError} when the browser cannot host
   * the runtime (no cross-origin isolation, no SharedArrayBuffer, …).
   * @throws {@link BootFailedError} when the runtime module cannot be loaded
   * or the kernel fails to boot.
   */
  static async boot(options: BootOptions = {}): Promise<Container> {
    const failures = environmentFailures();
    if (failures.length > 0) {
      throw new EnvironmentUnsupportedError(failures.map((f) => f.message));
    }
    if (Container.bootedInstance && !Container.bootedInstance.destroyed) {
      throw new BootFailedError(
        'A Nodine container is already booted in this page. Call teardown() ' +
          'on the existing instance before booting a new one.',
      );
    }
    if (options.coep === 'none') {
      throw new EnvironmentUnsupportedError([
        'coep: "none" — Nodine requires cross-origin isolation (SharedArrayBuffer + WASM threads); there is no non-isolated fallback mode',
      ]);
    }
    // runtimeUrl is optional: the default matches the URL served by the
    // Nodine Vite plugin (and produced by `npx nodine-stage public/nodine`).
    const runtimeUrlRaw = options.runtimeUrl == null ? DEFAULT_RUNTIME_URL : String(options.runtimeUrl);

    // Resolve the runtime URL to an absolute URL up front, so that (a) the
    // dynamic import works from any context and (b) runtimeBase (used for
    // sibling assets like sw-nodine.js) is exact. Pages resolve against
    // document.baseURI; Workers against their own location. A Worker whose
    // script is a blob: (e.g. the VS Code web extension host) cannot resolve
    // a relative URL at all — require an absolute one in that case.
    const resolveRuntimeUrl = (): URL => {
      try {
        return new URL(runtimeUrlRaw);
      } catch {
        // relative — resolve against the embedding context
      }
      const base =
        typeof document !== 'undefined'
          ? document.baseURI
          : typeof self !== 'undefined' && self.location
            ? self.location.href
            : undefined;
      if (!base) {
        throw new BootFailedError(
          `runtimeUrl "${runtimeUrlRaw}" is relative and this context (a blob: Worker) has no base URL — pass an absolute URL (e.g. https://host/nodine/supervisor.mjs)`,
        );
      }
      try {
        return new URL(runtimeUrlRaw, base);
      } catch {
        throw new BootFailedError(
          `runtimeUrl "${runtimeUrlRaw}" cannot be resolved against ${base} — pass an absolute URL`,
        );
      }
    };
    const runtimeUrl = resolveRuntimeUrl();

    const term = new HeadlessTermSink();
    const statusEls = {
      kernel: { textContent: '' },
      procs: { textContent: '' },
      cwd: { textContent: '' },
      exit: { textContent: '' },
      seed: { textContent: '' },
    };

    let mod;
    try {
      mod = await loadRuntimeModule(runtimeUrl);
    } catch (e) {
      throw new BootFailedError(e instanceof Error ? e.message : String(e));
    }

    const supervisor = new mod.Supervisor(term, statusEls, options.perf ? { perf: true } : undefined);
    // BootOptions.env (public API): page-owned base env vars layered over
    // the runtime's own baseEnv — host-side persistence for knobs like
    // NODINE_NETWORK_MODE that must reach every spawned process.
    if (options.env) {
      for (const [k, v] of Object.entries(options.env)) supervisor.baseEnv[k] = v;
    }
    try {
      assertRuntimeSurface(supervisor);
      await supervisor.boot();
    } catch (e) {
      throw new BootFailedError(
        'The Nodine kernel failed to boot: ' + (e instanceof Error ? e.message : String(e)),
      );
    }

    if (options.seedLibSources !== false) {
      try {
        await supervisor.seed();
      } catch (e) {
        // Boot without libsources is valid (bare kernel); report, don't fail.
        term.meta('[nodine] libsource seeding failed: ' + (e instanceof Error ? e.message : String(e)) + '\n');
      }
    }

    const workdirName = options.workdirName ?? 'project';
    const workdirPath = `/home/user/${workdirName}`;
    try {
      await supervisor.mkdirP(workdirPath);
    } catch {
      // fall back to the runtime's demo workspace if the name is unusable
    }

    // Resolve the runtime URL against the embedding context's base: pages
    // have document.baseURI; Workers use their own location.
    const container = new Container(supervisor, term, workdirPath, runtimeUrl);
    container.__sup = supervisor;

    if (options.enablePreview !== false) {
      await container.initPreviewBridge(options.forwardPreviewErrors);
    }

    Container.bootedInstance = container;
    return container;
  }

  private destroyed = false;

  /** Debug/back-reference to the runtime supervisor (internal, browser devtools). */
  __sup: RuntimeSupervisor | null = null;

  /** Preview session id, set once the preview SW bridge is established. */
  private previewSid: string | null = null;

  private readonly handlers: EventHandlers = {
    'server-ready': new Set(),
    port: new Set(),
    error: new Set(),
    'preview-message': new Set(),
  };

  private processes = new Set<ContainerProcessImpl>();

  private async initPreviewBridge(forwardPreviewErrors?: boolean | 'exceptions-only'): Promise<void> {
    if (!('serviceWorker' in navigator)) return;
    try {
      // The preview gateway ships inside the runtime bundle: resolve it
      // relative to the runtime module, NOT the page base, so the runtime
      // can be served from any directory. NOTE the '/' scope: the SW must
      // intercept origin-rooted /preview/<sid>/<port>/… URLs, so the script
      // must live at the origin root (or the server must send
      // Service-Worker-Allowed: / for it). If registration fails, boot
      // continues and preview events simply never fire.
      const swUrl = new URL('sw-nodine.js', this.runtimeBase).href;
      const reg = await navigator.serviceWorker.register(swUrl, { scope: '/' });
      await navigator.serviceWorker.ready;
      let sw = navigator.serviceWorker.controller || reg.active || reg.installing || reg.waiting;
      for (let i = 0; i < 50 && !sw; i++) {
        await new Promise((r) => setTimeout(r, 100));
        sw = reg.active || navigator.serviceWorker.controller;
      }
      if (!sw) return;
      const sid = crypto.randomUUID();
      const { port1, port2 } = new MessageChannel();
      this.supervisor.initPreviewSW(sid, port1);
      // M1.5: tell the SW whether this session wants preview error
      // forwarding. 'all' = exceptions + console.error; 'exceptions' =
      // uncaught errors + rejections only; absent = no injection at all
      // (the documented default — hosts opt in).
      const forwardErrors = forwardPreviewErrors === true ? 'all'
        : forwardPreviewErrors === 'exceptions-only' ? 'exceptions'
          : forwardPreviewErrors === false || forwardPreviewErrors === undefined ? null
            : null;
      sw.postMessage(
        { type: 'preview-bridge', sid, forwardErrors },
        [port2],
      );
      this.previewSid = sid;

      // Route supervisor preview events to SDK listeners.
      this.supervisor.onPreviewEvent = (ev) => {
        const url = this.previewUrl(ev.port);
        if (ev.kind === 'open') {
          for (const fn of this.handlers['server-ready']) {
            try { fn(ev.port, url); } catch { /* listener errors are the host's */ }
          }
        }
        for (const fn of this.handlers.port) {
          try { fn(ev.port, ev.kind, url); } catch { }
        }
      };

      // M1.5: preview-error events from the SW (captured inside preview
      // documents by the injected hook) → 'preview-message' listeners. The
      // SW already validated session + port + shape; here we map onto the
      // public contract's message types. Invalid payloads are dropped — a
      // malformed event must never reach host listeners.
      this.supervisor.onPreviewMessage = (m) => {
        if (!m || m.sid !== this.previewSid) return;
        const base = {
          previewId: this.previewSid ?? '',
          port: m.port,
          pathname: typeof m.pathname === 'string' ? m.pathname : '',
          search: typeof m.search === 'string' ? m.search : '',
          hash: typeof m.hash === 'string' ? m.hash : '',
        };
        let msg: PreviewMessage | null = null;
        if (m.kind === 'uncaught') {
          msg = {
            ...base,
            type: 'PREVIEW_UNCAUGHT_EXCEPTION',
            message: String(m.message ?? ''),
            stack: typeof m.stack === 'string' ? m.stack : '',
          };
        } else if (m.kind === 'rejection') {
          msg = {
            ...base,
            type: 'PREVIEW_UNHANDLED_REJECTION',
            message: String(m.message ?? ''),
            stack: typeof m.stack === 'string' ? m.stack : '',
          };
        } else if (m.kind === 'console') {
          msg = {
            ...base,
            type: 'PREVIEW_CONSOLE_ERROR',
            message: String(m.message ?? ''),
          };
        }
        if (!msg) return;
        for (const fn of this.handlers['preview-message']) {
          try { fn(msg); } catch { /* listener errors are the host's */ }
        }
      };
    } catch {
      // Preview is best-effort in M1.2: no SW → no events, container works.
    }
  }

  /** Absolute URL for a preview port (same-origin gateway path). */
  previewUrl(port: number): string {
    if (!this.previewSid) return `/preview/unavailable/${port}/`;
    return `/preview/${this.previewSid}/${port}/`;
  }

  private assertLive(): void {
    if (this.destroyed) throw new ContainerDestroyedError();
  }

  private resolvePath(p: string): string {
    this.assertLive();
    if (!p) return this.workdirPath;
    return p.startsWith('/') ? this.supervisor.normalizePath(p) : this.supervisor.abs(this.workdirPath, p);
  }

  /** The default value of the PATH environment variable for spawned processes. */
  get path(): string {
    this.assertLive();
    return this.supervisor.baseEnv.PATH ?? '/usr/bin:/bin';
  }

  /**
   * The runtime's base environment (HOME, PATH, …) as a plain record.
   * Additive public API (batch 3 validation): hosts layer their session
   * env over this without touching private supervisor internals. Returns
   * a copy — mutating it has no effect on the runtime.
   */
  get baseEnv(): Record<string, string> {
    this.assertLive();
    return { ...this.supervisor.baseEnv };
  }

  /** The full path of the container's working directory. */
  get workdir(): string {
    this.assertLive();
    return this.workdirPath;
  }

  private fsInstance: FileSystemAPI | null = null;

  /** Access to the container filesystem. */
  get fs(): FileSystemAPI {
    this.assertLive();
    if (!this.fsInstance) {
      this.fsInstance = createFileSystem(this.supervisor, (p) => this.resolvePath(p));
    }
    return this.fsInstance;
  }

  /** Subscribe to raw terminal output (all processes, ANSI included). */
  onOutput(fn: (chunk: string) => void): Unsubscribe {
    this.assertLive();
    return this.term.onTap(fn);
  }

  /**
   * Mounts a tree of files into the container filesystem relative to the
   * container working directory.
   */
  async mount(tree: FileSystemTree): Promise<void> {
    this.assertLive();
    const errors: string[] = [];
    await this.mountNode(tree, this.workdirPath, errors);
    if (errors.length > 0) {
      throw new FileSystemError('mount failed: ' + errors.join('; '));
    }
  }

  private async mountNode(node: FileSystemTree, dir: string, errors: string[]): Promise<void> {
    for (const [name, value] of Object.entries(node)) {
      const target = dir === '/' ? '/' + name : dir + '/' + name;
      try {
        if ('file' in value) {
          const data = typeof value.file.contents === 'string'
            ? new TextEncoder().encode(value.file.contents)
            : value.file.contents;
          await this.supervisor.kwriteFile(target, data, 0o644);
        } else if ('symlink' in value) {
          await this.supervisor.mkdirP(this.supervisor.parentOf(target));
          await this.supervisor.ksymlink(value.symlink, target);
        } else if ('directory' in value) {
          await this.supervisor.mkdirP(target);
          await this.mountNode(value.directory, target, errors);
        }
      } catch (e) {
        errors.push(`${target}: ${e instanceof Error ? e.message : String(e)}`);
      }
    }
  }

  /**
   * Spawns a process inside the container.
   *
   * @remarks
   * The process runs with piped IO: feed its stdin through `input`, read
   * stdout/stderr through `output`. Output contains only this process's
   * bytes: its own piped writes plus inherit-mode descendants, which the
   * runtime re-emits onto this process's pipe (dup semantics). Output from
   * other processes never appears here — the container-wide terminal view
   * is {@link Container.onOutput}.
   */
  async spawn(
    command: string,
    args: string[] = [],
    options: SpawnOptions = {},
  ): Promise<ContainerProcess> {
    this.assertLive();
    const argv = [command, ...args];

    const env: Record<string, string> = { ...this.supervisor.baseEnv };
    if (options.env) {
      for (const [k, v] of Object.entries(options.env)) {
        env[k] = String(v);
      }
    }
    const cwd = options.cwd
      ? (options.cwd.startsWith('/')
          ? this.supervisor.normalizePath(options.cwd)
          : this.supervisor.abs(this.workdirPath, options.cwd))
      : this.workdirPath;

    // Piped stdio for the SDK: fd0 pipe (channel stdin), fd1/2 pipe.
    // M1.4 isolation: because fd1/2 are piped, the runtime's child-process
    // bridge maps this process's inherit-mode descendants onto the SAME
    // pipes (dup semantics) instead of the global kernel FIFO — every byte
    // this stream sees belongs to this process tree and nothing else.
    // stdio:'inherit' (batch 3 validation): terminal-attached mode — the
    // process tree writes to the shared terminal stream (onOutput), like
    // the POC terminal. Needed for /bin/sh: the runtime currently does not
    // re-emit inherit-mode grandchildren onto a pipe, so shell pipelines
    // are invisible in pipe mode (known runtime limitation).
    const stdioMode = options.stdio === 'inherit'
      ? 'inherit'
      : options.output === false ? ['ignore', 'ignore', 'ignore'] : ['pipe', 'pipe', 'pipe'];

    // Synthetic parent holding one end of a dedicated channel: the supervisor
    // posts spawn-result (viaPort) and child-stdout/stderr/exit (parent.port)
    // to the parent's end — which is port2 here, because MessageChannel
    // delivers to the PARTNER of the port being posted to. The SDK listens on
    // port1. (Listening on the same end being posted to delivers nothing.)
    const { port1, port2 } = new MessageChannel();
    const syntheticParent = {
      pid: -1,
      worker: null,
      port: port2,
      ep: { index: -1, generation: -1 },
      parent: null,
      children: new Set<number>(),
      stdioMode: 'pipe',
      argv: [],
      entry: null,
      env,
      cwd,
      ready: true,
      exited: false,
      parentPid: null,
    } as unknown as RuntimeProcessRecord;

    let spawnError: string | null = null;
    port1.onmessage = (ev: MessageEvent) => {
      const m = ev.data as { type: string; error?: string };
      if (m.type === 'spawn-result' && m.error) spawnError = m.error;
    };

    let proc: RuntimeProcessRecord | null = null;
    try {
      proc = await this.supervisor.spawnProcess({
        argv,
        env,
        cwd,
        stdioMode,
        parent: syntheticParent,
        viaPort: port2,
      });
    } catch (e) {
      port1.close(); port2.close();
      throw new SpawnError(
        `spawn ${command}: ${e instanceof Error ? e.message : String(e)}`,
      );
    }

    if (!proc) {
      port1.close(); port2.close();
      // resolveExec failures for root spawns print to the terminal; recover
      // the message from the headless sink (most recent line).
      throw new SpawnError(
        spawnError ??
        `spawn ${command}: command could not be resolved (command not found or exec format error)`,
      );
    }

    const impl = new ContainerProcessImpl(
      proc,
      this.supervisor,
      this.term,
      port1,
      port2,
    );
    this.processes.add(impl);
    void impl.exit.then(() => this.processes.delete(impl));
    return impl;
  }

  /**
   * Destroys the container, releasing its resources. All derived entities
   * (processes, the filesystem view) become unusable.
   */
  async teardown(): Promise<void> {
    if (this.destroyed) return;
    this.destroyed = true;
    for (const p of this.processes) {
      try { p.kill(); } catch { }
    }
    try {
      await this.supervisor.shutdown(0);
    } catch {
      // shutdown is best-effort; the runtime tolerates double shutdown.
    }
    this.supervisor.onPreviewEvent = null;
    this.supervisor.onPreviewMessage = null;
    // M1.5: tell the preview SW this session is gone — it drops the session
    // registrations and its bridge port, so still-open preview tabs stop
    // forwarding hook messages (and nothing leaks for the next boot).
    if (this.previewSid && 'serviceWorker' in navigator) {
      try {
        const sw = navigator.serviceWorker.controller;
        if (sw) sw.postMessage({ type: 'preview-bridge-end', sid: this.previewSid });
      } catch { /* best-effort: the SW may already be gone */ }
    }
    if (Container.bootedInstance === this) {
      Container.bootedInstance = null;
    }
  }

  /**
   * Registers an event listener. Returns a function that unsubscribes the
   * listener.
   */
  on(event: 'server-ready', listener: ServerReadyListener): Unsubscribe;
  on(event: 'port', listener: PortListener): Unsubscribe;
  on(event: 'error', listener: ErrorListener): Unsubscribe;
  on(event: 'preview-message', listener: PreviewMessageListener): Unsubscribe;
  on(
    event: ContainerEventName,
    listener:
      | ServerReadyListener
      | PortListener
      | ErrorListener
      | PreviewMessageListener,
  ): Unsubscribe {
    this.assertLive();
    const set = this.handlers[event] as Set<unknown>;
    set.add(listener);
    return () => { set.delete(listener); };
  }
}

/**
 * ContainerProcess implementation over a real runtime process record.
 * input → channel stdin-bytes; output ← this process's channel only
 * (child-stdout/child-stderr — own piped writes + inherit descendants the
 * runtime re-emits onto the pipe; M1.4: no shared-terminal tap, concurrent
 * processes are fully isolated); kill → signalTree SIGTERM; resize is not
 * yet backed (kernel is fixed 80×24).
 */
class ContainerProcessImpl implements ContainerProcess {
  readonly exit: Promise<number>;
  readonly input: WritableStream<string>;
  readonly output: ReadableStream<string>;
  private killed = false;

  constructor(
    private readonly proc: RuntimeProcessRecord,
    private readonly supervisor: RuntimeSupervisor,
    _term: HeadlessTermSink, // M1.4: process output no longer taps the shared sink (kept for call-shape stability)
    port1: MessagePort,
    port2: MessagePort,
  ) {
    // --- exit promise: child-exit on the synthetic-parent channel ---------
    let resolveExit!: (code: number) => void;
    this.exit = new Promise<number>((res) => { resolveExit = res; });

    // --- output: push controller over both sources ------------------------
    let controller!: ReadableStreamDefaultController<string>;
    this.output = new ReadableStream<string>({
      start(c) { controller = c; },
      cancel() { /* listener went away; keep buffers internal */ },
    });
    const pushText = (text: string) => {
      try { controller.enqueue(text); } catch { /* stream closed */ }
    };

    // (a) channel tap: this process's own piped stdout/stderr bytes.
    port1.onmessage = (ev: MessageEvent) => {
      const m = ev.data as {
        type: string; pid?: number; code?: number; bytes?: ArrayBuffer | Uint8Array;
        error?: string; eof?: boolean;
      };
      if (m.type === 'child-stdout' || m.type === 'child-stderr') {
        if (m.bytes) {
          const u8 = m.bytes instanceof Uint8Array
            ? m.bytes
            : new Uint8Array(m.bytes);
          pushText(new TextDecoder().decode(u8));
        }
        return;
      }
      if (m.type === 'child-exit') {
        this.lastKnownCode = m.code ?? 0;
        settleExit(this.lastKnownCode);
        return;
      }
      if (m.type === 'spawn-result' && m.error) {
        // a descendant failed to spawn; report as error text on the stream
        pushText(`[nodine] spawn failed: ${m.error}\n`);
      }
    };

    // (b) M1.4: the terminal-sink tap is GONE. It forwarded the shared
    // kernel-FIFO drain (inherit-mode descendants, global terminal output)
    // into EVERY live process's stream — with concurrent spawns each
    // process received the others' output (observed live, M1.4 diagnosis).
    // output is now strictly this process's own channel: its piped
    // stdout/stderr plus inherit-mode descendants (the worker's child
    // bridge re-emits their bytes onto this process's pipe — dup
    // semantics). The container-wide terminal view remains available via
    // container.onOutput().

    // Safety: if child-exit never arrives (worker crash), resolve exit from
    // the proc record. NEVER close ports here: handleExit sets proc.exited
    // synchronously BEFORE the queued child-stdout/child-exit port messages
    // are dispatched on the next macrotask; closing the port here would
    // discard that queue (fast processes would lose their final output —
    // observed live before this fix).
    let exitSettled = false;
    const settleExit = (code: number) => {
      if (exitSettled) return;
      exitSettled = true;
      resolveExit(code);
    };
    const poll = setInterval(() => {
      if (this.proc.exited) {
        clearInterval(poll);
        settleExit(this.lastKnownCode ?? 0);
      }
    }, 250);
    // Final cleanup on exit resolution: give the port queue a macrotask to
    // deliver trailing chunks, then close ports and the output stream.
    void this.exit.then(() => {
      clearInterval(poll);
      setTimeout(() => {
        this.closePorts();
        try { controller.close(); } catch { }
      }, 50);
    });

    // --- input: channel stdin-bytes (piped fd0) ---------------------------
    this.input = new WritableStream<string>({
      write: (chunk) => {
        if (this.proc.exited) return;
        this.proc.port.postMessage(
          { type: 'stdin-bytes', bytes: new TextEncoder().encode(chunk) },
          // structured-clone copies the view; no transfer needed
        );
      },
      close: () => {
        if (!this.proc.exited) {
          try { this.proc.port.postMessage({ type: 'stdin-eof' }); } catch { }
        }
      },
    });

    this.port1 = port1;
    this.port2 = port2;
  }

  private readonly port1: MessagePort;
  private readonly port2: MessagePort;
  private lastKnownCode: number | null = null;

  private closedPorts = false;
  private closePorts(): void {
    if (this.closedPorts) return;
    this.closedPorts = true;
    try { this.port1.close(); } catch { }
    try { this.port2.close(); } catch { }
  }

  /** Kills the process (SIGTERM to the process tree). */
  kill(): void {
    if (this.proc.exited || this.killed) return;
    this.killed = true;
    try { this.supervisor.signalTree(this.proc, 'SIGTERM'); } catch { }
    // Fallback: SIGKILL after 3 s if the tree ignores SIGTERM.
    setTimeout(() => {
      if (!this.proc.exited) {
        try { this.supervisor.signalTree(this.proc, 'SIGKILL'); } catch { }
      }
    }, 3000);
  }

  /**
   * Sends SIGINT to the process tree (Ctrl+C semantics: the foreground
   * process group gets a chance to exit cleanly, handlers run). Unlike
   * kill() this is NOT fatal-by-design and does not escalate — a process
   * that ignores SIGINT keeps running. Additive public API (batch 3
   * validation): the website terminal needs real Ctrl+C without touching
   * private supervisor internals.
   */
  interrupt(): void {
    if (this.proc.exited) return;
    try { this.supervisor.signalTree(this.proc, 'SIGINT'); } catch { }
  }

  /** Resizes the attached terminal. Not yet backed by the runtime. */
  resize(_dimensions: TerminalDimensions): void {
    throw new NotYetImplementedError(
      'process.resize (real PTY sizing arrives with the TTY milestone; the kernel is currently fixed at 80×24)',
    );
  }
}
