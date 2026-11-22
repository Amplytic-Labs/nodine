# Nodine architecture — the merged single package

Status: this is the package-merged successor of the former private-core /
`@nodine/api`-SDK split (documented historically in the old repository as
`docs/architecture/nodine-package-architecture.md`). One npm package now
contains both layers; there is no distribution service and no tunneling.

## The one-paragraph picture

A host page (your app) imports the SDK from `nodine` and boots it with a
`runtimeUrl` pointing at the staged `runtime/` bundle. The SDK loads
`supervisor.mjs` into the page; the supervisor boots a Rust/WASM
microkernel (`kernel_core.wasm`) in a dedicated `kernel-driver.js` worker
over a shared `WebAssembly.Memory`, fetches the vendored Node.js v26.13.0
library sources (`libsources.json`) once per page, seeds a complete `/usr`
filesystem image into the kernel VFS from `npm-seed-blob` (npm, `/bin/sh`,
coreutils, pako, @noble/hashes), and then spawns every process as its own
`runtime-worker.mjs` module worker — where the genuine Node bootstrap chain
runs verbatim on the browser's JS engine against the kernel's syscalls.
`http.Server.listen()` inside any process is bridged onto a same-origin
service worker (`sw-opencontainer.js`) so dev servers get real preview URLs
at `/preview/<sid>/<port>/…`. There is no host relay and no tunnel: outbound
HTTP is the browser's own `fetch` (direct mode), inbound HTTP is the SW
bridge, and without it `listen()` dark-listens (succeeds with no transport).

## Layers (and where they live)

| Layer | Files | Notes |
| --- | --- | --- |
| Public API SDK | `src/` → `dist/` | TypeScript, browser-side. `Container.boot/spawn/fs/on/teardown`. No runtime logic — it adapts the supervisor. |
| Supervisor | `runtime/supervisor.mjs` | Page-side: kernel boot, VFS seeding, process tree, terminal sink, preview port registry. Loaded by the SDK via `import(runtimeUrl)`. |
| Process workers | `runtime/runtime-worker.mjs` | One module worker per process. Boots Node's bootstrap chain from libsources; implements the syscall ABI against the kernel; the fetch-bridged network stack; the `PortServer` HTTP transport. Frozen wire contract: `docs/runtime-worker-contract.md`. |
| Kernel driver | `runtime/kernel-driver.js` | Bootstraps the WASM kernel in a worker; owns the shared-memory RPC mailbox. |
| Microkernel | `runtime/kernel_core.wasm`, source in the `kernel/` Rust crates (`kernel-abi`, `kernel-core`, `kernel-vfs`) | ABI v1.0.2 frozen transport; VFS with inodes, content store, accounting. |
| Preview gateway | `runtime/sw-opencontainer.js` | Service worker, scope `/`. Per-session (`sid`) port registry; per-port MessagePort channels; root-relative subresource rewriting. |
| Node sources | `runtime/libsources.json` (derived from `vendor/node-v26.13.0/lib`) | Verbatim upstream ids (extensionless); the undici browser shim and the internal/deps dists are injected at build time. |
| `/usr` image | `runtime/npm-seed-blob` + `runtime/npm-manifest.json` (derived from `vendor/deps/*.tgz` + `vendor/coreutils`) | CSB1 one-shot blob (preferred) with per-file manifest fallback. |
| Worker-imported ESM | `runtime/unpkg/{pako,noble,jsmd5}/` (derived) | The worker imports pako / @noble/hashes / js-md5 directly by URL. |

Derived artifacts are **committed**: a fresh clone is always
`npm publish`-ready. `npm run build:runtime` regenerates them from
`vendor/` (deterministic; the `/bin/sh` dependency closure needs one
network npm install of pinned versions — `scripts/build-shell-deps.cjs`).

## Boot sequence

1. `Container.boot(options)` — environment gate (cross-origin isolation,
   SharedArrayBuffer, WASM threads, secure context); loads the supervisor
   module from `runtimeUrl`; verifies its surface.
2. `supervisor.boot()` — starts `kernel-driver.js`, fetches
   `kernel_core.wasm`, allocates the shared `WebAssembly.Memory`, boots the
   kernel, begins the one-shot `libsources.json` fetch + parse.
3. `supervisor.seed()` — streams `npm-seed-blob` into the VFS (one
   request; falls back to per-file `npm-manifest.json` + `/npm-file`
   fetches), then writes the demo workspace.
4. SDK registers `sw-opencontainer.js` (scope `/`), opens a MessagePort
   to it (`preview-bridge`), and calls `supervisor.initPreviewSW(sid, port)`.
5. SDK creates the working directory; the container is live.

Everything resolves relative to `supervisor.mjs`'s own URL
(`import.meta.url`): workers, `kernel_core.wasm`, `libsources.json`,
`npm-seed-blob`, `uvmap.json` (worker-relative), and the `unpkg/` tree
(worker-relative). The bundle therefore works from any directory; only the
preview service worker's `/` scope needs the bundle at the origin root (or
a `Service-Worker-Allowed: /` header).

## Networking

- Outbound (npm installs, `fetch`, HTTP clients): a fetch-bridged stack.
  Default `direct` mode sends allowlisted anonymous HTTPS GET/HEAD straight
  to the registry from the browser. `proxy` mode (opt-in via
  `BootOptions.env.CARAPACE_NETWORK_MODE`) routes through a host `/egress`
  relay.
- Inbound (`http.Server.listen`): the SW bridge is the only transport.
  Without it (or when `enablePreview: false`), `listen()` dark-listens —
  it succeeds and emits `listening`, but no external connection can
  arrive. No WebSockets in previews (SW cannot intercept WS), and there
  is deliberately no public/remote tunnel.

## Deliberate omissions (recorded so nobody re-adds them blind)

- **Tunneling is gone.** The former Cloudflare Worker+DO public tunnel
  gateway, the page-side `tunnel-bridge`, the supervisor's
  `initTunnelBridge`, the worker's `preview-ws-*` upgrade handlers and the
  legacy `/port-tunnel` WebSocket transport were removed in this merge.
  Preview = same-origin SW, or nothing. If remote exposure is ever
  needed, design it as a separate host-side component.
- **pnpm-wasm** is not part of this artifact set (the runtime keeps the
  worker-side node-support shims; the @pnpm/wasm dist is not shipped).
- **The distribution service** (signed manifests, artifact CDN) from the
  old architecture docs is not implemented; the package *is* the artifact.
