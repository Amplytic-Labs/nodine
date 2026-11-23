#!/usr/bin/env node
// smoke-vite.cjs — the "install → one plugin line → boot" gate.
//
// Scaffolds a REAL vite project in a scratch dir (vite from the registry,
// nodine from the packed tarball), boots the runtime through the official
// plugin with ZERO boot options (no runtimeUrl, no headers config), drives
// it in headless chromium, then runs `vite build` and verifies the runtime
// bundle lands in the build output.
//
// Run from the repo root: node scripts/smoke-vite.cjs
'use strict';
const { execFileSync, spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const http = require('node:http');

const ROOT = path.resolve(__dirname, '..');
const PORT = 8920;
let dev = null; // the vite dev process (killed on every exit path)

const waitPort = (port, ms = 30000) => new Promise((res, rej) => {
  const t0 = Date.now();
  const probe = () => {
    http.get({ host: '127.0.0.1', port, path: '/' }, (r) => { r.resume(); res(); })
      .on('error', () => {
        if (Date.now() - t0 > ms) return rej(new Error('vite dev server did not come up'));
        setTimeout(probe, 300);
      });
  };
  probe();
});

const INDEX_HTML = `<!doctype html>
<html><head><meta charset="utf-8"><title>nodine vite smoke</title></head>
<body><script type="module">
import { Container } from '@amplytic/nodine';
const results = [];
window.__smoke = { done: false, results };
const check = (name, pass, extra) => results.push({ name, pass: !!pass, extra: extra ?? null });
// surface the exact SW registration URL + failure reason
const origRegister = navigator.serviceWorker.register.bind(navigator.serviceWorker);
navigator.serviceWorker.register = async (url, opts) => {
  try { return await origRegister(url, opts); }
  catch (e) { check('register error', false, String(url) + ' :: ' + String(e && e.message || e)); throw e; }
};
(async () => {
  let c = null;
  try {
    check('cross-origin isolated', window.crossOriginIsolated === true);
    c = await Container.boot(); // ZERO options: plugin path + default runtimeUrl
    check('boot with zero options', true);
    // SW diagnostics: the preview bridge depends on the registration
    const regs = await navigator.serviceWorker.getRegistrations();
    check('sw registration', regs.length > 0, JSON.stringify(regs.map((r) => ({
      scope: r.scope, active: !!r.active, installing: !!r.installing, waiting: !!r.waiting,
    }))));
    check('sw controller', !!navigator.serviceWorker.controller || regs.some((r) => !!r.active),
      'controller=' + !!navigator.serviceWorker.controller);
    await c.mount({ 'hi.js': { file: { contents: 'console.log("hi from vite smoke")' } } });
    const p = await c.spawn('node', ['hi.js']);
    let out = '';
    const rd = p.output.getReader();
    const dl = Date.now() + 20000;
    let exited = false; p.exit.then(() => { exited = true; }).catch(() => {});
    while (Date.now() < dl) {
      const r = await Promise.race([rd.read(), new Promise((r) => setTimeout(() => r('t'), 400))]);
      if (r === 't') { if (exited && out) break; continue; }
      if (r.done) break;
      out += r.value;
      if (exited) break;
    }
    check('spawn output', out.includes('hi from vite smoke'), JSON.stringify(out.slice(0, 80)));
    check('exit code', (await p.exit) === 0);
    let preview = null;
    const off = c.on('server-ready', (port, url) => { preview = url; off(); });
    const srv = await c.spawn('node', ['-e', 'require("http").createServer((q,r)=>r.end("vite ok")).listen(4173)']);
    const dl2 = Date.now() + 20000;
    while (Date.now() < dl2 && !preview) await new Promise((r) => setTimeout(r, 200));
    check('preview event', !!preview, JSON.stringify(preview));
    if (preview) {
      const resp = await fetch(preview).then((r) => r.text().then((t) => ({ s: r.status, t }))).catch((e) => ({ s: 0, t: String(e) }));
      check('preview fetch', resp.s === 200 && resp.t === 'vite ok', JSON.stringify(resp).slice(0, 80));
    }
    try { srv.kill(); } catch {}
    await c.teardown();
    check('teardown', true);
  } catch (e) {
    check('harness', false, String(e && e.message || e));
  }
  window.__smoke.done = true;
})();
</script></body></html>`;

const VITE_CONFIG = `import { defineConfig } from 'vite';
import { nodine } from '@amplytic/nodine/vite';
export default defineConfig({ plugins: [nodine()] });
`;

(async () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'nodine-vite-'));
  console.log('[vite-smoke] scratch:', scratch);

  // 1. pack
  execFileSync('npm', ['pack', '--silent'], { cwd: ROOT, stdio: 'pipe' });
  const { name: pkgName, version: pkgVersion } = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'));
  const tgzName = `${pkgName.replace(/^@/, '').replace(/\//g, '-')}-${pkgVersion}.tgz`;
  const tgz = path.join(ROOT, tgzName);
  if (!fs.existsSync(tgz)) throw new Error(`pack did not produce ${tgzName}`);
  console.log('[vite-smoke] packed:', fs.statSync(tgz).size, 'bytes');

  // 2. real vite project
  const app = path.join(scratch, 'app');
  fs.mkdirSync(app);
  fs.writeFileSync(path.join(app, 'package.json'), JSON.stringify({ name: 'vite-smoke', private: true, type: 'module' }));
  fs.writeFileSync(path.join(app, 'index.html'), INDEX_HTML);
  fs.writeFileSync(path.join(app, 'vite.config.mjs'), VITE_CONFIG);
  console.log('[vite-smoke] installing nodine tarball + vite (from the registry)…');
  execFileSync('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error', tgz], { cwd: app, stdio: 'inherit' });
  execFileSync('npm', ['install', '--no-audit', '--no-fund', '--no-save', '--loglevel=error', 'vite'], { cwd: app, stdio: 'inherit' });
  const viteBin = path.join(app, 'node_modules', '.bin', 'vite');
  if (!fs.existsSync(viteBin)) throw new Error('vite binary missing after install');
  console.log('[vite-smoke] vite installed');

  // 3. dev server (bind IPv4 explicitly so the probe + playwright agree)
  dev = spawn(viteBin, ['--host', '127.0.0.1', '--port', String(PORT), '--strictPort'], { cwd: app, stdio: ['ignore', 'pipe', 'pipe'] });
  const devLog = [];
  dev.stdout.on('data', (d) => { devLog.push(String(d)); });
  dev.stderr.on('data', (d) => devLog.push('[err] ' + String(d)));
  try {
    await waitPort(PORT);
  } catch (e) {
    dev.kill('SIGKILL');
    console.error('[vite-smoke] vite dev failed to start:\n' + devLog.join(''));
    throw e;
  }
  console.log('[vite-smoke] vite dev up on :' + PORT + ' (headers + /nodine/ from the plugin)');

  // 4. drive it
  const { chromium } = require('playwright');
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error' || m.type() === 'warning') console.log('[page:' + m.type() + ']', m.text().slice(0, 250)); });
  page.on('response', (r) => {
    const u = r.url();
    if (u.includes('sw-nodine') || /\/(preview)\//.test(u)) {
      console.log('[resp]', r.status(), '"' + (r.headers()['content-type'] || '') + '"', u.slice(0, 130));
    }
  });
  page.on('response', async (r) => {
    if (r.url().includes('/nodine/supervisor.mjs')) {
      const h = r.headers();
      const ct = h['content-type'] || '';
      const coi = (h['cross-origin-opener-policy'] || '') + '/' + (h['cross-origin-embedder-policy'] || '');
      console.log(`[vite-smoke] supervisor.mjs served: status=${r.status()} ct="${ct}" coi="${coi}"`);
    }
  });
  await page.goto(`http://127.0.0.1:${PORT}/`, { waitUntil: 'domcontentloaded' });
  const deadline = Date.now() + 180000;
  for (;;) {
    const done = await page.evaluate(() => window.__smoke && window.__smoke.done).catch(() => false);
    if (done || Date.now() > deadline) break;
    await page.waitForTimeout(500);
  }
  const state = await page.evaluate(() => window.__smoke ? { done: window.__smoke.done, results: window.__smoke.results } : null);
  await browser.close();
  if (!state) throw new Error('no smoke state');
  let failed = 0;
  for (const r of state.results) {
    console.log(`  ${r.pass ? 'PASS' : 'FAIL'}  ${r.name}${r.extra != null && r.extra !== true ? ' — ' + String(r.extra).slice(0, 140) : ''}`);
    if (!r.pass) failed++;
  }
  if (!state.done) { console.log('[vite-smoke] TIMED OUT\n' + devLog.slice(-15).join('')); failed++; }
  if (errors.length) console.log('[vite-smoke] page errors:', errors.slice(0, 3).join(' | ').slice(0, 400));

  // 5. production build contains the runtime bundle
  execFileSync(viteBin, ['build'], { cwd: app, stdio: 'pipe' });
  for (const f of ['nodine/supervisor.mjs', 'nodine/kernel_core.wasm', 'nodine/npm-seed-blob', 'nodine/sw-nodine.js', 'nodine/libsources.json']) {
    if (!fs.existsSync(path.join(app, 'dist', f))) { console.log('  FAIL  build output missing ' + f); failed++; }
  }
  if (!failed) console.log('  PASS  vite build output contains the runtime bundle (dist/nodine/…)');

  dev.kill('SIGKILL');
  fs.rmSync(scratch, { recursive: true, force: true });
  fs.rmSync(tgz, { force: true });
  console.log(failed === 0 ? 'VITE SMOKE OK — install, plugin, boot: no staging, no header config' : 'VITE SMOKE FAILED (' + failed + ')');
  process.exit(failed === 0 ? 0 : 1);
})().catch(async (e) => {
  console.error('[vite-smoke] failed:', e && e.message || e);
  try { if (dev) dev.kill('SIGKILL'); } catch {}
  process.exit(1);
});
