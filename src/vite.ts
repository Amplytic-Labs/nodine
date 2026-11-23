/**
 * The Vite plugin for Nodine — the zero-setup integration.
 *
 * ```js
 * // vite.config.js
 * import { nodine } from 'nodine/vite';
 * export default { plugins: [nodine()] };
 * ```
 * ```
 *
 * That single line gives you, in `vite dev` and `vite preview`:
 * - **Cross-origin isolation headers** on every response (COOP `same-origin`
 *   + COEP `credentialless`), which the runtime needs for
 *   SharedArrayBuffer + WASM threads. No server config to write.
 * - **The runtime bundle served from `node_modules/nodine/runtime`** at
 *   `/nodine/…` with the right MIME types and a
 *   `Service-Worker-Allowed: /` header so the preview service worker can
 *   register at scope `/`. Nothing to copy, nothing to stage.
 *
 * So in the app code the boot call needs no options at all:
 *
 * ```js
 * import { Container } from 'nodine';
 * const container = await Container.boot(); // runtimeUrl defaults to
 *                                           // /nodine/supervisor.mjs
 * ```
 *
 * And `vite build` copies the runtime bundle into the build output
 * (`dist/nodine/…`) so the production bundle is complete; the only thing a
 * static host adds is the two isolation headers (see the README's deploy
 * table — they are a hard browser requirement for SharedArrayBuffer).
 *
 * Options (all optional):
 * - `path` — URL path prefix to serve the runtime under. Default
 *   `'/nodine'`. Use `'/'` to serve it at the origin root (then no
 *   `Service-Worker-Allowed` header is ever needed in production).
 * - `coep` — the COEP header value to send. Default `'credentialless'`
 *   (dev-friendly: cross-origin subresources still load), `'require-corp'`
 *   for the stricter classic mode.
 *
 * @module
 */

import { fileURLToPath } from 'node:url';
import fs from 'node:fs';
import path from 'node:path';

/** Options for {@link nodine}. @public */
export interface NodinePluginOptions {
  /** URL path prefix the runtime bundle is served under. Default '/nodine'. */
  path?: string;
  /** COEP header value: 'credentialless' (default) or 'require-corp'. */
  coep?: 'credentialless' | 'require-corp';
}

// Minimal structural view of the Vite server/plugin surface. Value-only
// Node imports keep this file's public .d.ts free of Node/Vite types.
interface ConnectRequest { url?: string }
interface ConnectResponse {
  setHeader(name: string, value: string): void;
  statusCode: number;
  end(chunk?: string | Buffer): void;
}
interface ConnectNext { (err?: unknown): void }
interface Middleware { (req: ConnectRequest, res: ConnectResponse, next: ConnectNext): void }
interface ViteServer { middlewares: { use(mw: Middleware): void } }
interface ViteBuildConfig { build?: { outDir?: string } }
interface ViteResolvedConfig { build: { outDir?: string } }

/** Where this package ships its runtime bundle from (…/nodine/runtime). */
const RUNTIME_DIR = fileURLToPath(new URL('../runtime', import.meta.url));

const MIME: Record<string, string> = {
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.wasm': 'application/wasm',
  '.txt': 'text/plain; charset=utf-8',
};

// Top-level names inside the runtime bundle. Needed when `path: '/'`: the
// plugin must serve the bundle's files at the root WITHOUT shadowing the
// app's own root files, so only these names are intercepted.
const RUNTIME_TOP_NAMES = new Set([
  'supervisor.mjs', 'runtime-worker.mjs', 'kernel-driver.js', 'kernel_core.wasm',
  'sync-module-wrap.mjs', 'acorn-vendor.mjs', 'sw-nodine.js', 'uvmap.json',
  'libsources.json', 'npm-manifest.json', 'npm-seed-blob', 'runtime-version.txt',
  'unpkg',
]);

function normalizePrefix(p: string | undefined): string {
  let out = p == null || p === '' ? '/nodine' : p;
  if (!out.startsWith('/')) out = '/' + out;
  while (out.length > 1 && out.endsWith('/')) out = out.slice(0, -1);
  if (out === '/') return '';
  return out;
}

/**
 * Create the Nodine Vite plugin. See the module doc for the one-line setup.
 *
 * @public
 */
export function nodine(options: NodinePluginOptions = {}): object {
  const prefix = normalizePrefix(options.path);
  const coep = options.coep === 'require-corp' ? 'require-corp' : 'credentialless';
  let outDir: string | null = null;

  const middleware: Middleware = (req, res, next) => {
    // 1. Cross-origin isolation on every response — the page needs it to
    //    host the runtime, and it must be present on the document itself,
    //    so it is set before any handler writes the response.
    res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
    res.setHeader('Cross-Origin-Embedder-Policy', coep);

    // 2. Serve the runtime bundle straight from node_modules (no staging).
    const url = req.url ?? '';
    let isRuntimeAsset: boolean;
    if (prefix === '') {
      const first = url.split('?')[0].replace(/^\/+/, '').split('/')[0];
      isRuntimeAsset = RUNTIME_TOP_NAMES.has(first);
    } else {
      isRuntimeAsset = url.startsWith(prefix + '/');
    }
    if (!isRuntimeAsset) {
      return next();
    }
    const rel = (prefix === '' ? url : url.slice(prefix.length)).split('?')[0].replace(/^\/+/, '');
    if (rel.includes('..')) { res.statusCode = 400; res.end('bad path'); return; }
    const file = path.join(RUNTIME_DIR, rel);
    if (!file.startsWith(RUNTIME_DIR + path.sep) || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
      res.statusCode = 404;
      res.end('nodine runtime asset not found: ' + rel);
      return;
    }
    res.setHeader('Content-Type', MIME[path.extname(file)] || 'application/octet-stream');
    // The preview service worker registers at scope '/' — allow it from
    // wherever the bundle is served.
    res.setHeader('Service-Worker-Allowed', '/');
    // Runtime modules must never come from cache (they carry a content
    // stamp, but dev correctness beats caching here).
    res.setHeader('Cache-Control', 'no-store');
    fs.createReadStream(file).pipe(res as unknown as NodeJS.WritableStream);
  };

  return {
    name: 'nodine',
    configResolved(config: ViteResolvedConfig) {
      outDir = (config as ViteBuildConfig).build?.outDir || null;
    },
    configureServer(server: ViteServer) {
      server.middlewares.use(middleware);
    },
    configurePreviewServer(server: ViteServer) {
      server.middlewares.use(middleware);
    },
    closeBundle() {
      // Copy the runtime bundle into the build output so `vite build`
      // produces a deployable tree (dist/<path>/…). Merging (no wipe) when
      // the bundle is served at the root — the outDir belongs to the app.
      if (!outDir) return;
      const dest = prefix === '' ? outDir : path.join(outDir, prefix);
      if (prefix !== '') fs.rmSync(dest, { recursive: true, force: true });
      fs.cpSync(RUNTIME_DIR, dest, { recursive: true });
      console.log(`[nodine] runtime bundle copied to ${dest} — serve COOP/COEP headers in production (see the README deploy table)`);
    },
  };
}
