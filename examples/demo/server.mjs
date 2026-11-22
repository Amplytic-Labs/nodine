#!/usr/bin/env node
// examples/demo/server.mjs — the demo's static dev server.
//
// Serves, on ONE origin (headers matter — the runtime needs cross-origin
// isolation):
//   /                -> examples/demo/index.html
//   /e2e.html        -> test/e2e.html            (the SDK E2E page)
//   /sdk/*           -> dist/*                   (the built SDK, ESM tree)
//   /supervisor.mjs, /runtime-worker.mjs, /kernel_core.wasm, …
//                    -> runtime/*                (the bundle, at the ORIGIN
//                                                ROOT — the recommended
//                                                layout so the preview
//                                                service worker's '/' scope
//                                                is allowed)
//   /unpkg/...       -> runtime/unpkg/...
//
// Run: node examples/demo/server.mjs [port]     (default 8899)
// Then open http://localhost:8899/

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '../..');
const PORT = Number(process.argv[2] || 8899);

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.wasm': 'application/wasm',
  '.txt': 'text/plain; charset=utf-8',
};

const routes = {
  '/': path.join(HERE, 'index.html'),
  '/e2e.html': path.join(ROOT, 'test/e2e.html'),
};

const TREES = [
  { prefix: '/sdk/', dir: path.join(ROOT, 'dist') },
  { prefix: '/', dir: path.join(ROOT, 'runtime') },
];

const server = http.createServer((req, res) => {
  const u = new URL(req.url, 'http://x');
  let file = routes[u.pathname];
  if (!file) {
    for (const { prefix, dir } of TREES) {
      if (!u.pathname.startsWith(prefix)) continue;
      const rel = u.pathname.slice(prefix.length);
      // path-safe join: reject traversal
      if (rel.includes('..')) break;
      const target = path.join(dir, rel);
      if (target.startsWith(dir + path.sep) && fs.existsSync(target) && fs.statSync(target).isFile()) {
        file = target;
        break;
      }
    }
  }
  if (!file || !fs.existsSync(file)) {
    res.writeHead(404, { 'content-type': 'text/plain' });
    res.end('not found: ' + u.pathname);
    return;
  }
  const headers = {
    // cross-origin isolation — REQUIRED by the runtime (SharedArrayBuffer +
    // WASM threads). A real deployment sets these on its CDN/host too.
    'Cross-Origin-Opener-Policy': 'same-origin',
    'Cross-Origin-Embedder-Policy': 'require-corp',
    'Cross-Origin-Resource-Policy': 'same-origin',
    'Cache-Control': 'no-store',
    'Content-Type': MIME[path.extname(file)] || 'application/octet-stream',
  };
  res.writeHead(200, headers);
  fs.createReadStream(file).pipe(res);
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`nodine demo: http://localhost:${PORT}/  (demo page)`);
  console.log(`nodine demo: http://localhost:${PORT}/e2e.html  (SDK E2E page)`);
  console.log(`runtime bundle: ${path.join(ROOT, 'runtime')} (served at origin root)`);
});
