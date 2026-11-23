#!/usr/bin/env node
// smoke-expo.cjs — the end-to-end product test the user asked for:
//   inside a booted Nodine container (real browser):
//     1. npx create-expo-app@latest myapp   (full npm registry install)
//     2. cd myapp && npm start              (expo -> Metro on :8081)
//     3. fetch the preview URL /status      (packager-status:running)
// Run from the repo root: node scripts/smoke-expo.cjs   (long: up to ~30 min)
'use strict';
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const { execFile } = require('node:child_process');

const ROOT = path.resolve(__dirname, '..');
const PORT = 8930;
const MIME = { '.js': 'text/javascript', '.mjs': 'text/javascript', '.json': 'application/json', '.wasm': 'application/wasm', '.html': 'text/html', '.txt': 'text/plain' };

// minimal COI static server: repo at origin root (runtime at /, SDK dist at /dist)
const server = http.createServer((req, res) => {
  res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
  res.setHeader('Cross-Origin-Embedder-Policy', 'credentialless');
  res.setHeader('Service-Worker-Allowed', '/');
  let p = req.url.split('?')[0];
  if (p === '/') p = '/expo-harness.html';
  const file = path.join(ROOT, p.replace(/^\/+/, ''));
  if (!file.startsWith(ROOT) || !fs.existsSync(file) || !fs.statSync(file).isFile()) { res.statusCode = 404; return res.end('nf'); }
  res.setHeader('Content-Type', MIME[path.extname(file)] || 'application/octet-stream');
  fs.createReadStream(file).pipe(res);
});

const HARNESS = `<!doctype html><html><body><script type="module">
import { Container } from '/dist/index.js';
const S = { phase: 'booting', done: false, results: [], log: [], preview: null, createExit: null, startExit: null };
window.__expo = S;
const check = (name, pass, extra) => S.results.push({ name, pass: !!pass, extra: extra ?? null });
const log = (line) => { S.log.push(line); if (S.log.length > 400) S.log.splice(0, S.log.length - 400); };
const readAll = (proc, tag) => { const rd = proc.output.getReader(); (async () => { for (;;) { const r = await rd.read(); if (r.done) return; log(tag + r.value.replace(/\\x1b\\[[0-9;?]*[A-Za-z]/g, '')); } })(); };
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  try {
    const c = await Container.boot({ runtimeUrl: '/runtime/supervisor.mjs' });
    check('boot', true);
    log('workdir=' + c.workdir);

    // 1. create the app (npx, falling back to the equivalent npm exec)
    S.phase = 'create-expo-app';
    let create;
    try {
      create = await c.spawn('npx', ['create-expo-app@latest', 'myapp'], { cwd: c.workdir, env: { CI: '1', EXPO_NO_TELEMETRY: '1' } });
    } catch (e) {
      log('npx spawn failed (' + String(e && e.message || e).slice(0, 120) + ') — retrying via npm exec');
      create = await c.spawn('npm', ['exec', '--yes', 'create-expo-app@latest', '--', 'myapp'], { cwd: c.workdir, env: { CI: '1', EXPO_NO_TELEMETRY: '1' } });
    }
    readAll(create, '[create] ');
    S.createExit = await Promise.race([create.exit, wait(20 * 60 * 1000).then(() => 'TIMEOUT')]);
    if (S.createExit !== 0) { check('create-expo-app exit 0', false, 'exit=' + S.createExit); }
    else {
      check('create-expo-app exit 0', true);
      const app = await c.fs.readdir(c.workdir + '/myapp').catch(() => null);
      check('myapp/ created with files', !!app && app.length > 3, app ? app.slice(0, 12).join(',') : 'missing');
      log('myapp entries: ' + (app ? app.length : 'MISSING'));

      // 2. start metro
      S.phase = 'npm start (expo -> metro :8081)';
      let preview = null;
      const off = c.on('server-ready', (port, url) => { if (port === 8081) { preview = url; off(); } });
      const start = await c.spawn('npm', ['--prefix', 'myapp', 'start'], { cwd: c.workdir, env: { CI: '1', EXPO_NO_TELEMETRY: '1' } });
      readAll(start, '[start] ');
      const dl = Date.now() + 8 * 60 * 1000;
      while (Date.now() < dl && preview == null) await wait(500);
      check('metro listening on 8081 (server-ready)', !!preview, preview || 'no event in 8 min');
      if (preview) {
        S.preview = preview;
        await wait(3000); // let the bundler settle
        const r = await fetch(preview + 'status').then((x) => x.text().then((t) => ({ s: x.status, t }))).catch((e) => ({ s: 0, t: String(e) }));
        check('GET /status through preview', r.s === 200 && r.t.includes('packager-status:running'), JSON.stringify(r).slice(0, 120));
      }
      try { start.kill(); } catch {}
    }
    S.phase = 'done';
  } catch (e) {
    check('harness', false, String(e && e.message || e).slice(0, 200));
    S.phase = 'error';
  }
  S.done = true;
})();
</script></body></html>`;
fs.writeFileSync(path.join(ROOT, 'expo-harness.html'), HARNESS);

(async () => {
  await new Promise((r) => server.listen(PORT, '127.0.0.1', r));
  console.log('[expo-smoke] COI server on :' + PORT);
  const { chromium } = require('playwright');
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  page.on('pageerror', (e) => console.log('[pageerror]', String(e).slice(0, 400)));
  page.on('response', (r) => { if (r.status() >= 400) console.log('[http ' + r.status() + ']', r.url().slice(0, 160)); });
  page.on('console', (m) => { const t = m.text(); if (t.length < 300) console.log('[page:' + m.type() + ']', t.slice(0, 280)); });
  await page.goto('http://127.0.0.1:' + PORT + '/', { waitUntil: 'domcontentloaded' });

  let lastLen = 0;
  for (;;) {
    await new Promise((r) => setTimeout(r, 10000));
    const st = await page.evaluate(() => window.__expo ? {
      phase: window.__expo.phase, done: window.__expo.done, n: window.__expo.log.length,
      tail: window.__expo.log.slice(-4), results: window.__expo.results,
    } : { missing: true }).catch((e) => ({ lost: String(e && e.message || e).slice(0, 120) }));
    if (st.lost || st.missing) {
      console.log(st.missing ? '[expo-smoke] __expo missing — harness script never ran' : '[expo-smoke] page lost: ' + st.lost);
      const html = await page.content().catch(() => '');
      console.log('[page html]', html.slice(0, 600).replace(/\n/g, ' '));
      break;
    }
    if (st.n > lastLen) { for (const l of st.tail) console.log('  >', l.slice(0, 220)); lastLen = st.n; }
    console.log('[tick] phase=' + st.phase + ' log=' + st.n);
    if (st.done) {
      let failed = 0;
      for (const r of st.results) { console.log((r.pass ? '  PASS ' : '  FAIL ') + r.name + (r.extra != null ? ' — ' + String(r.extra).slice(0, 160) : '')); if (!r.pass) failed++; }
      console.log(failed ? 'EXPO SMOKE FAILED (' + failed + ')' : 'EXPO SMOKE OK — create-expo-app + npm start + metro preview all work in-browser');
      await browser.close(); server.close();
      fs.rmSync(path.join(ROOT, 'expo-harness.html'), { force: true });
      process.exit(failed ? 1 : 0);
    }
  }
  process.exit(1);
})().catch((e) => { console.error('[expo-smoke] driver failed:', e && e.message || e); process.exit(1); });
