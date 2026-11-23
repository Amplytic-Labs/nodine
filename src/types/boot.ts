/**
 * Options for {@link Container.boot}.
 *
 * @public
 */
export interface BootOptions {
  /**
   * The name of the working directory created inside the container.
   *
   * @remarks
   * This is a cosmetic option: it changes the folder name that
   * {@link Container.workdir} reports (for example `/home/project` instead of
   * an auto-generated name). If omitted, a default name is chosen.
   */
  workdirName?: string;

  /**
   * URL of the `supervisor.mjs` module of the Nodine runtime bundle.
   * OPTIONAL: when omitted, the SDK boots from
   * {@link DEFAULT_RUNTIME_URL} (`/nodine/supervisor.mjs`) — the path the
   * official Vite plugin (`nodine/vite`) serves the bundle at, and the
   * location `npx nodine-stage public/nodine` stages it to.
   *
   * @remarks
   * The runtime resolves ALL of its assets (workers, `kernel_core.wasm`,
   * `libsources.json`, the npm seed blob, the `unpkg/` tree) relative to
   * this module's own URL, so the bundle can live in any directory the host
   * serves. Two caveats:
   *
   * - The host origin must send COOP/COEP headers (the runtime needs
   *   cross-origin isolation).
   * - For previews (`enablePreview`), the preview service worker registers
   *   with scope `/` — so either serve the bundle at the origin root or
   *   send a `Service-Worker-Allowed: /` header for `sw-nodine.js`.
   */
  runtimeUrl?: string | URL;

  /**
   * Whether to seed the container's VFS with the distributed Node.js library
   * sources (required for `npm install` and most real tools). Defaults to
   * true. Disabling it boots a bare kernel with an empty `/usr` tree.
   */
  seedLibSources?: boolean;

  /**
   * Register the preview service worker and bridge preview ports, enabling
   * `server-ready` / `port` events and `/preview/…` URLs. Defaults to true.
   *
   * @remarks
   * Requires a secure context (https or localhost). When registration fails
   * the container still boots; preview events simply never fire.
   */
  enablePreview?: boolean;

  /**
   * The Cross-Origin-Embedder-Policy value the embedding page is served with.
   *
   * @remarks
   * Nodine requires SharedArrayBuffer-based cross-origin isolation, so the
   * embedding application MUST serve COOP `same-origin` plus a COEP value.
   * This option tells the SDK which COEP mode the host uses so it can verify
   * the environment correctly:
   *
   * - `'require-corp'` — classic COEP; every cross-origin subresource needs
   *   CORP or CORS headers.
   * - `'credentialless'` — modern COEP; cross-origin loads are sent without
   *   credentials instead.
   *
   * `'none'` (no isolation) is NOT supported by Nodine: the runtime relies on
   * `SharedArrayBuffer` and WASM threads, which require cross-origin
   * isolation. Passing `'none'` makes boot fail with
   * {@link EnvironmentUnsupportedError}.
   */
  coep?: 'require-corp' | 'credentialless' | 'none';

  /**
   * Whether errors occurring in preview iframes should be forwarded to the
   * embedding page as `preview-message` events.
   *
   * @defaultValue `false`
   *
   * @remarks
   * When enabled, Nodine injects an error-capture script into preview pages;
   * captured `console.error` calls, uncaught exceptions and unhandled
   * rejections are then emitted as {@link PreviewMessage} events. With
   * `'exceptions-only'`, `console.error` calls are not forwarded.
   */
  forwardPreviewErrors?: boolean | 'exceptions-only';

  /**
   * Enable runtime performance instrumentation (diagnostics).
   *
   * @defaultValue `false`
   *
   * @remarks
   * When enabled, the runtime collects per-process timelines (spawn → worker
   * ready → exit), per-stage bootstrap timings, per-op RPC latency
   * histograms and npm phase timers, and pushes them to the supervisor's
   * `perfData` record. This is a diagnostics knob for integration testing
   * and performance work; it adds small overhead per kernel op and should
   * stay off in production.
   */
  perf?: boolean;

  /**
   * Base environment variables merged into every spawned process's
   * environment, layered over the runtime's own base environment.
   *
   * @remarks
   * Host-side persistence for environment knobs the embedding page owns —
   * most importantly `NODINE_NETWORK_MODE` ('direct' | 'proxy'), which
   * selects how the runtime's fetch bridge reaches package registries.
   * The DEFAULT is 'direct' (this package ships self-contained on static
   * hosts): allowlisted anonymous HTTPS GET/HEAD requests go straight from
   * the browser to the registry, with no server-side relay. 'proxy' opts
   * into routing non-safelisted requests through a same-origin `/egress`
   * relay that the host must then serve. Only set 'proxy' if you run such
   * a relay.
   */
  env?: Record<string, string>;
}
