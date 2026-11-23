# Nodine

A **real Node.js runtime that runs entirely inside the browser tab** — no
server round-trips, no cloud VMs, no fake terminal.

Nodine boots a **Rust/WASM microkernel** in a Web Worker, runs **genuine
upstream Node.js v26.13.0** (vendored lib sources booted verbatim on the
browser's own JS engine, one process per Web Worker), ships a complete
`/usr` tree with a **working `npm`**, and exposes it through a small typed
API: mount files, spawn processes, read their output, and watch dev servers
come up on same-origin preview URLs.

Everything — the filesystem, processes, `npm install`, dev servers — runs
**locally in the user's browser**.

## Install

```sh
npm install @amplytic/nodine
```

## Quickstart (Vite)

Add the plugin to your `vite.config.js`:

```js
import { defineConfig } from 'vite';
import { nodine } from '@amplytic/nodine/vite';

export default defineConfig({
  plugins: [nodine()],
});
```

Boot it in your app:

```ts
import { Container } from '@amplytic/nodine';

const container = await Container.boot();

// mount files
await container.mount({
  'package.json': { file: { contents: JSON.stringify({ name: 'demo', private: true }) } },
  'main.js': { file: { contents: 'console.log("hello from", process.version)\n' } },
});

// spawn a real Node.js process and stream its output
const proc = await container.spawn('node', ['main.js']);
proc.output.pipeTo(new WritableStream({ write: (d) => console.log(d) }));
await proc.exit;

// npm works — in the browser
const install = await container.spawn('npm', ['install', 'left-pad']);
install.output.pipeTo(new WritableStream({ write: (d) => console.log(d) }));
await install.exit;
```

That's it. The plugin takes care of everything the runtime needs on your
dev server:

- **Cross-origin isolation headers** (COOP/COEP) on every response —
  required for SharedArrayBuffer + WASM threads.
- **Serving the runtime bundle** from `node_modules/nodine/runtime` at
  `/nodine/…` with the right MIME types and a
  `Service-Worker-Allowed: /` header so previews work.
- **`vite build` copies the runtime bundle** into your build output
  (`dist/nodine/…`) so production deploys are complete.

For a runnable end-to-end example (terminal-style UI, file writes,
processes, preview server, `npm install`), see
[`examples/demo/`](examples/demo) — one HTML file plus a tiny dev server.

## Quickstart (any other setup)

The runtime bundle is a static directory. Two things must be true for it to
boot, no matter what serves it:

1. Your page's origin sends the isolation headers (a browser requirement —
   SharedArrayBuffer is gated on them):
   ```
   Cross-Origin-Opener-Policy: same-origin
   Cross-Origin-Embedder-Policy: credentialless    (or require-corp)
   ```
2. The bundle is reachable at a URL you pass as `runtimeUrl`.

The easiest way is the bundled stager:

```sh
npx nodine-stage public/nodine
```

```ts
const container = await Container.boot(); // default runtimeUrl: /nodine/supervisor.mjs
```

Any other location works too — just say where:

```ts
const container = await Container.boot({ runtimeUrl: '/runtime/supervisor.mjs' });
```

## The API

### `Container`

- `Container.boot(options?): Promise<Container>` — boot the runtime.
  Single instance per page; `teardown()` to release.
- `container.fs: FileSystemAPI` — `mkdir`, `readdir`, `readFile`,
  `writeFile`, `rename`, `rm`, `watch` (modeled on Node's `fs.promises`).
- `container.mount(tree: FileSystemTree)` — mount files, directories and
  symlinks in one call.
- `container.spawn(command, args?, options?): Promise<ContainerProcess>` —
  spawn a process with piped stdio. Spawned programs see a real
  `process`, `Buffer`, `require`, `fs`, `http`… everything the vendored
  Node.js library provides.
- `container.on(event, listener): () => void` — `server-ready`, `port`,
  `error`, `preview-message`; returns an unsubscribe function.
- `container.onOutput(fn): () => void` — tap the raw terminal output
  stream (all processes, ANSI included).
- `container.previewUrl(port): string` — same-origin URL of a preview port.
- `container.teardown()` — destroy the instance and release resources.
- `container.workdir: string`, `container.path: string`,
  `container.baseEnv: Record<string, string>`.

### `BootOptions`

| Option | Default | Notes |
| --- | --- | --- |
| `runtimeUrl` | `'/nodine/supervisor.mjs'` | Where `supervisor.mjs` is served. The default matches the Vite plugin and `nodine-stage public/nodine`. The runtime resolves all its assets relative to this module, so any location works. |
| `coep` | — (any but `'none'`) | `'credentialless'` or `'require-corp'`; `'none'` is rejected — there is no non-isolated fallback. |
| `workdirName` | `'project'` | Name of the working directory (`/home/user/<name>`). |
| `enablePreview` | `true` | Register the preview service worker (see below). `false` → dev servers "dark-listen": they boot and print their banners, but preview URLs are unavailable. |
| `seedLibSources` | `true` | Seed the vendored Node lib sources + npm tree. `false` boots a bare kernel. |
| `forwardPreviewErrors` | `false` | `true` / `'exceptions-only'`: forward `console.error` / uncaught errors from preview iframes as `preview-message` events. |
| `env` | — | Extra base env vars layered into every spawned process. |
| `perf` | `false` | Runtime instrumentation (diagnostics only). |

### Process output isolation

Every process has an **independent output stream** — `process.output`
receives only bytes produced by that process's own tree. A child with
`stdio: 'inherit'` inside an SDK-spawned process shares the parent's pipe
(`dup` semantics). `container.onOutput()` remains the container-wide view.

## Previews

With `enablePreview` (the default), the SDK registers a **same-origin
service worker** and bridges every `http.Server.listen()` inside the runtime
onto it. Dev servers (Vite, Metro/expo, webpack-serve,
`http.createServer`) get real preview URLs at:

```
/preview/<session-id>/<port>/<path>
```

- `server-ready` / `port` events fire with the preview URL.
- Previews are reachable from your origin only; WebSocket upgrades are not
  passed through (service workers cannot intercept WebSocket traffic).
- Service workers need a secure context (https or localhost). When
  registration fails, the container still boots — dev servers keep working
  (banner + interactive mode), preview events just never fire.
- The service worker registers with scope `/`, so the bundle can live in
  any directory as long as the server sends
  `Service-Worker-Allowed: /` on `sw-nodine.js` (the Vite plugin
  does). Or serve the bundle at the origin root — then no extra header is
  needed anywhere.

**Cache-busting:** runtime modules are cached by the browser. On every
re-stage, fetch the fresh stamp and append it, so users never boot a stale
runtime:

```ts
const v = await (await fetch('/nodine/runtime-version.txt', { cache: 'no-store' })).text();
const container = await Container.boot({ runtimeUrl: `/nodine/supervisor.mjs?v=${v.trim()}` });
```

## Networking inside the container

HTTP from spawned processes goes through a fetch-bridged network stack.
The default mode is `direct`: allowlisted anonymous HTTPS GET/HEAD requests
(npm registry and friends) go straight from the browser to the registry —
no server-side relay needed. Hosts that want to proxy egress can set
`env: { NODINE_NETWORK_MODE: 'proxy' }` and serve a same-origin `/egress`
relay.

Known limits, stated plainly:

- Piped stdin has no EOF signal; `resize()` is unbacked (80×24).
- `fs.watch` and `export()` are not implemented.
- UDP sockets (`dgram`) are unsupported; `worker_threads` is a shim
  (browser-Worker-backed, enough for the packages Nodine tests with).
- `pnpm` is not part of this artifact set.
- The runtime requires a Chromium-class browser (cross-origin isolation +
  WASM threads). `Container.boot()` fails loudly with
  `EnvironmentUnsupportedError` listing exactly what is missing.

## Deploying to production

`vite build` already puts the runtime bundle in `dist/nodine/`. The only
thing a static host adds is the two isolation headers — a hard browser
requirement for SharedArrayBuffer, not a Nodine policy:

| Host | Config |
| --- | --- |
| nginx | `add_header Cross-Origin-Opener-Policy same-origin; add_header Cross-Origin-Embedder-Policy credentialless;` in the server/location block |
| Vercel | `vercel.json`: `{"headers":[{"source":"/(.*)","headers":[{"key":"Cross-Origin-Opener-Policy","value":"same-origin"},{"key":"Cross-Origin-Embedder-Policy","value":"credentialless"}]}]}` |
| Netlify | `netlify.toml`: `[[headers]]` `for = "/*"` then `Cross-Origin-Opener-Policy = "same-origin"`, `Cross-Origin-Embedder-Policy = "credentialless"` |
| Express | `app.use((req, res, next) => { res.setHeader('Cross-Origin-Opener-Policy', 'same-origin'); res.setHeader('Cross-Origin-Embedder-Policy', 'credentialless'); next(); })` |
| Node (dev) | `node examples/demo/server.mjs` — sends the right headers and serves the bundle |

If the bundle is served outside the origin root (for example
`/nodine/…`), also add `Service-Worker-Allowed: /` for
`sw-nodine.js` (or serve the bundle at the root and skip this).

## Repository layout

```
├── src/            TypeScript API SDK + the Vite plugin (compiles to dist/)
├── runtime/        the static runtime bundle — served by the plugin or stage it
│   ├── supervisor.mjs        page-side process supervisor
│   ├── runtime-worker.mjs    Node.js execution worker (one per process)
│   ├── kernel-driver.js     kernel bootstrapping worker
│   ├── kernel_core.wasm     the Rust/WASM microkernel (binary)
│   ├── sw-nodine.js  same-origin preview gateway (service worker)
│   ├── libsources.json       vendored Node v26.13.0 lib sources (derived)
│   ├── npm-seed-blob         /usr tree seed: npm, /bin/sh, coreutils (derived)
│   └── unpkg/                pako / @noble/hashes / js-md5 ESM (derived)
├── kernel/         Rust source of the microkernel + the frozen ABI spec
├── vendor/         upstream inputs (Node lib sources, dep tarballs, shims —
│                   see vendor/README.md)
├── scripts/        build-runtime.mjs, stage-runtime.mjs (nodine-stage),
│                   smoke-install.cjs, smoke-vite.cjs
├── test/           node --test contracts + adapter tests; browser E2E
├── examples/demo/  runnable end-to-end demo page + COI dev server
└── docs/           architecture + per-subsystem deep dives
```

The derived artifacts (marked above) are **committed**, so a fresh clone is
always publish-ready. `npm run build:runtime` regenerates them from
`vendor/` (deterministic; one network npm install of pinned versions for
the `/bin/sh` dependency closure).

## Development

```sh
npm install       # devDependencies (TypeScript)
npm run build     # tsc -> dist/ + type tests
npm test          # contracts + adapter tests (node --test)
npm run check     # build everything + npm pack --dry-run
```

Browser-level verification (requires `playwright` with its `chromium`
browser installed):

```sh
npm run test:e2e     # 16-check SDK E2E: boot, fs, spawn, preview, teardown
npm run smoke        # npm pack -> install into a fresh project -> boot it
npm run smoke:vite   # install -> vite plugin line -> boot, with zero options
```

The kernel: `kernel/` is a Cargo workspace building
`runtime/kernel_core.wasm` for `wasm32-unknown-unknown` with the frozen ABI
v1.0.2 transport (see `docs/runtime-worker-contract.md` for the wire
protocol between supervisor, workers and kernel).

## License

@amplytic/nodine is licensed under the **Mozilla Public License 2.0** — see [LICENSE](./LICENSE).
If you modify this package, you must share those modifications under MPL-2.0;
software that integrates or uses nodine is not covered by that requirement.