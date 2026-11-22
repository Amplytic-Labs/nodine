# Nodine

A **real Node.js runtime that runs entirely inside the browser tab** — no
server round-trips, no cloud VMs, no fake terminal.

Nodine boots a **Rust/WASM microkernel** in a Web Worker, runs **genuine
upstream Node.js v26.13.0** (vendored lib sources booted verbatim on the
browser's own JS engine, one process per Web Worker), seeds a complete
`/usr` tree with a **working `npm`**, and exposes it all through a small
typed API: mount files, spawn processes, read their output, and watch dev
servers come up on same-origin preview URLs.

Everything — the filesystem, the processes, `npm install`, the dev servers —
executes **locally in the user's browser**. The package never calls a
Nodine server for runtime operations.

> One package. This repo used to split a private "core" and a public
> `@nodine/api` SDK (the SDK loaded the core from a distribution service
> that never shipped). That split is gone: `npm install nodine` gives you
> the API SDK *and* the full runtime bundle, and nothing else is needed.
> The same-origin preview gateway is the only outbound surface — there is
> **no remote tunneling** (and no Cloudflare tunnel gateway).

## Install

```sh
npm install nodine
```

## Quickstart

**1. Stage the runtime bundle** into your static file tree (it must be
served by your own server, same origin as your page):

```sh
npx nodine-stage public/nodine
```

(or `npx nodine-stage --root public` to serve it at the origin root — the
recommended layout, see [Previews](#previews))

**2. Serve cross-origin-isolation headers.** Nodine needs
`SharedArrayBuffer` + WASM threads, so your server must send on every
response of the embedding origin:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: credentialless    (or require-corp)
```

**3. Boot it:**

```ts
import { Container } from 'nodine';

const container = await Container.boot({
  runtimeUrl: '/nodine/supervisor.mjs',
  coep: 'credentialless',
  workdirName: 'project',
});

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
install.output.pipeTo(new WritableStream({ write: (d) => process.stdout.write(d) }));
await install.exit;
```

For a runnable end-to-end example (terminal UI, file writes, process
spawning, preview server, `npm install`), see [`examples/demo/`](examples/demo)
— one HTML file plus a tiny dev server that sends the required headers.

## The API

### `Container`

- `Container.boot(options?): Promise<Container>` — boot the runtime.
  Requires `runtimeUrl`. Single instance; `teardown()` to release.
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
| `runtimeUrl` | — (required) | URL of the staged bundle's `supervisor.mjs`. The runtime resolves **all** of its assets relative to this module, so the bundle works from any directory. |
| `coep` | — (any but `'none'`) | Which COEP value your host serves — `'credentialless'` or `'require-corp'` (isolation is verified directly either way); `'none'` is rejected: there is no non-isolated fallback. |
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

With `enablePreview` (the default), the SDK registers
`sw-opencontainer.js` — a **same-origin service worker** — and bridges
every `http.Server.listen()` inside the runtime onto it. Dev servers
(Vite, Metro/expo, webpack-serve, `http.createServer`) get real previews at:

```
/preview/<session-id>/<port>/<path>
```

- `server-ready` / `port` events fire with the preview URL.
- There is **no WebSocket pass-through** (WS bypasses service workers) and
  no remote tunneling: previews are reachable from the host origin only.
- The service worker registers with scope `/`. Serve the bundle at the
  origin root (`npx nodine-stage --root public`), or send
  `Service-Worker-Allowed: /` on `sw-opencontainer.js` when staging it in a
  subdirectory. Without it, boot still works — previews just never fire.
- Secure context (https or localhost) required for service workers.

**Cache-busting:** runtime modules are cached by the browser. On every
re-stage, fetch the fresh stamp and append it, so users never boot a
stale runtime:

```ts
const v = await (await fetch('/nodine/runtime-version.txt', { cache: 'no-store' })).text();
const container = await Container.boot({
  runtimeUrl: `/nodine/supervisor.mjs?v=${v.trim()}`,
  coep: 'credentialless',
});
```

## Networking inside the container

`fetch`/HTTP from spawned processes goes through a fetch-bridged network
stack. The default mode is `direct`: allowlisted anonymous HTTPS GET/HEAD
requests (npm registry and friends) go straight from the browser to the
registry — **no server-side relay needed**. Hosts that want to proxy
egress can set `env: { CARAPACE_NETWORK_MODE: 'proxy' }` and serve a
same-origin `/egress` relay.

Known limits, stated plainly:

- Piped stdin has no EOF signal; `resize()` is unbacked (80×24).
- `fs.watch` and `export()` are not implemented.
- UDP sockets (`dgram`) are unsupported; `worker_threads` is a shim
  (browser-Worker-backed, enough for the packages Nodine tests with).
- `pnpm` is not part of this artifact set.
- The runtime requires a Chromium-class browser (cross-origin isolation +
  WASM threads). `Container.boot()` fails loudly with
  `EnvironmentUnsupportedError` listing exactly what is missing.

## Requirements for embedding apps

- Serve the staged `runtime/` bundle from the **same origin** as your page.
- COOP `same-origin` + COEP (`credentialless` or `require-corp`) on every
  response of the page.
- Node.js >= 18 for the build/publish toolchain (`engines` field).

## Repository layout

```
├── src/            TypeScript API SDK (compiles to dist/)
├── runtime/        the static runtime bundle — stage it, serve it
│   ├── supervisor.mjs        page-side process supervisor
│   ├── runtime-worker.mjs    Node.js execution worker (one per process)
│   ├── kernel-driver.js     kernel bootstrapping worker
│   ├── kernel_core.wasm     the Rust/WASM microkernel (binary)
│   ├── sw-opencontainer.js  same-origin preview gateway (service worker)
│   ├── libsources.json       vendored Node v26.13.0 lib sources (derived)
│   ├── npm-seed-blob         /usr tree seed: npm, /bin/sh, coreutils (derived)
│   └── unpkg/                pako / @noble/hashes / js-md5 ESM (derived)
├── kernel/         Rust source of the microkernel (kernel-abi, kernel-core,
│                   kernel-vfs) + the frozen ABI spec (kernel/KERNEL_ABI.md)
│                   — builds kernel_core.wasm (gate: scripts/check-wasm-layout.sh)
├── vendor/         upstream inputs (Node lib sources, dep tarballs, shims —
│                   see vendor/README.md)
├── scripts/        build-runtime.mjs (regenerates the derived artifacts),
│                   stage-runtime.mjs (nodine-stage), shell patches
├── test/           node --test contracts + adapter tests
├── examples/demo/  runnable end-to-end demo page + COI dev server
└── docs/           architecture notes + the frozen runtime-worker contract
```

The derived artifacts (marked above) are **committed**, so a fresh clone is
always publish-ready. `npm run build:runtime` regenerates them from
`vendor/` (network access needed for the `/bin/sh` dependency closure);
the build is deterministic for the same inputs.

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
npm run test:e2e  # 16-check SDK E2E: boot, fs, spawn, preview, teardown
npm run smoke     # npm pack -> install into a fresh project -> boot it
```

The kernel: `kernel/` is a Cargo workspace building
`runtime/kernel_core.wasm` for `wasm32-unknown-unknown` with the frozen
ABI v1.0.2 transport (see `docs/runtime-worker-contract.md` for the wire
protocol between supervisor, workers and kernel).

## License

**AGPL-3.0-only** — see [LICENSE](LICENSE). Third-party components (Node.js
lib sources, npm, @yarnpkg/shell, acorn, pako, @noble/hashes, js-md5 and
the shell dependency closure) ship under their own licenses, itemized in
[NOTICE](NOTICE). "Nodine" and the Nodine logo are trademarks of Amplytic
Labs. This project is not affiliated with any other container-runtime
vendor.
