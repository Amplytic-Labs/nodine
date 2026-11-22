#!/usr/bin/env node
// test/e2e.cjs — SDK E2E driver. Boots examples/demo/server.mjs (COI
// headers, runtime at origin root), drives test/e2e.html in headless
// Chromium via playwright, and reports the check results.
//
// Requires: npm run build (dist/ exists), playwright's chromium available.
// Run: node test/e2e.cjs [port]        (default 8899)

'use strict';
const { chromium } = require('playwright');
const { spawn } = require('node:child_process');
const path = require('node:path');

const PORT = Number(process.argv[2] || 8899);
const HERE = __dirname;
const ROOT = path.resolve(HERE, '..');

(async () => {
  const server = spawn('node', [path.join(ROOT, 'examples/demo/server.mjs'), String(PORT)], { stdio: ['ignore', 'pipe', 'pipe'] });
  const serverLog = [];
  server.stdout.on('data', (d) => { const t = String(d); serverLog.push(t); if (/error|fatal/i.test(t)) console.log('[srv]', t.trim()); });
  server.stderr.on('data', (d) => console.error('[server-err]', String(d).slice(0, 300)));
  const serverReady = new Promise((res, rej) => {
    server.stdout.on('data', (d) => { if (String(d).includes(String(PORT))) res(); });
    setTimeout(() => rej(new Error('server boot timeout')), 15000);
  });
  await serverReady;
  console.log('[e2e] server up on :' + PORT);

  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  page.on('console', (m) => {
    const t = m.text();
    if (m.type() === 'error') pageErrors.push('console: ' + t);
    if (/\[nodine\]|SDK|preview|driver/i.test(t)) console.log('[page]', t.slice(0, 200));
  });

  const t0 = Date.now();
  await page.goto(`http://127.0.0.1:${PORT}/e2e.html`, { waitUntil: 'domcontentloaded' });

  // boot + seed + 6 check groups can legitimately take a while on cold caches
  const deadline = Date.now() + 180000;
  while (Date.now() < deadline) {
    const done = await page.evaluate(() => window.__e2e && window.__e2e.done);
    if (done) break;
    await page.waitForTimeout(500);
  }

  const state = await page.evaluate(() => window.__e2e ? { done: window.__e2e.done, results: window.__e2e.results } : null);
  await browser.close();
  server.kill('SIGTERM');

  if (!state || !state.done) {
    console.error('[e2e] TIMED OUT waiting for the page to finish');
    console.error('[e2e] server log tail:\n' + serverLog.slice(-20).join(''));
    process.exit(3);
  }

  let failed = 0;
  for (const r of state.results) {
    console.log(`  ${r.pass ? 'PASS' : 'FAIL'}  ${r.name}${r.extra != null && r.extra !== true ? ' — ' + String(r.extra).slice(0, 160) : ''}`);
    if (!r.pass) failed++;
  }
  console.log(`[e2e] ${state.results.length - failed}/${state.results.length} checks passed in ${Math.round((Date.now() - t0) / 1000)}s`);
  if (pageErrors.length) console.log('[e2e] page errors (informational): ' + pageErrors.slice(0, 5).join(' | ').slice(0, 500));
  process.exit(failed ? 1 : 0);
})().catch((e) => {
  console.error('[e2e] driver failed:', e && e.message || e);
  process.exit(2);
});
