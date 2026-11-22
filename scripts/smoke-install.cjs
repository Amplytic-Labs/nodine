#!/usr/bin/env node
// smoke-install.cjs — the "npm install and it just works" gate.
// 1. npm pack the package into a scratch dir
// 2. npm install the tarball into a fresh consumer project
// 3. start the demo server FROM THE INSTALLED PACKAGE
// 4. drive the demo page in headless chromium; require boot + spawn + preview
// Run from the repo root: node scripts/smoke-install.cjs
'use strict';
const { execFileSync, spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

const ROOT = path.resolve(__dirname, '..');
const PORT = 8910;

(async () => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'nodine-smoke-'));
  console.log('[smoke] scratch:', scratch);

  // 1. pack (name-version derived from package.json — no hardcoded filenames)
  execFileSync('npm', ['pack', '--silent'], { cwd: ROOT, stdio: 'pipe' });
  const { name: pkgName, version: pkgVersion } = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'));
  const tgz = path.join(ROOT, `${pkgName}-${pkgVersion}.tgz`);
  if (!fs.existsSync(tgz)) throw new Error(`pack did not produce ${pkgName}-${pkgVersion}.tgz`);
  console.log('[smoke] packed:', fs.statSync(tgz).size, 'bytes');

  // 2. fresh consumer project + install
  const app = path.join(scratch, 'consumer');
  fs.mkdirSync(app);
  fs.writeFileSync(path.join(app, 'package.json'), JSON.stringify({ name: 'consumer', private: true }));
  execFileSync('npm', ['install', '--no-audit', '--no-fund', tgz], { cwd: app, stdio: 'inherit' });
  const inst = path.join(app, 'node_modules', 'nodine');
  for (const f of ['dist/index.js', 'runtime/supervisor.mjs', 'runtime/kernel_core.wasm',
    'runtime/libsources.json', 'runtime/npm-seed-blob', 'scripts/stage-runtime.mjs',
    'examples/demo/server.mjs']) {
    if (!fs.existsSync(path.join(inst, f))) throw new Error('installed package missing: ' + f);
  }
  console.log('[smoke] installed package verified (dist + runtime + scripts + examples)');

  // 3. demo server from the installed tree
  const server = spawn('node', [path.join(inst, 'examples/demo/server.mjs'), String(PORT)], { stdio: ['ignore', 'pipe', 'pipe'] });
  const serverReady = new Promise((res, rej) => {
    server.stdout.on('data', (d) => { if (String(d).includes(String(PORT))) res(); });
    server.stderr.on('data', (d) => console.error('[smoke srv-err]', String(d).slice(0, 200)));
    setTimeout(() => rej(new Error('server boot timeout')), 10000);
  });
  await serverReady;
  console.log('[smoke] demo server up from node_modules on :' + PORT);

  // 4. drive it
  const { chromium } = require('playwright');
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(`http://127.0.0.1:${PORT}/`, { waitUntil: 'domcontentloaded' });
  await page.click('#run');
  await page.waitForSelector('#npm:not([disabled])', { timeout: 90000 });
  const lines = await page.$$eval('#log li', (els) => els.map((e) => ({ cls: e.className, text: e.textContent })));
  for (const l of lines) console.log('[smoke][' + l.cls + ']', l.text);
  const fails = lines.filter((l) => l.cls === 'fail');
  const boot = lines.find((l) => /booted/.test(l.text));
  const preview = lines.find((l) => /preview URL/.test(l.text));
  await browser.close();
  server.kill('SIGTERM');

  const pass = fails.length === 0 && !!boot && !!preview && errors.length === 0;
  if (errors.length) console.log('[smoke] page errors:', errors.slice(0, 3).join(' | '));
  console.log(pass ? 'SMOKE OK — npm install just works' : 'SMOKE FAILED');
  fs.rmSync(scratch, { recursive: true, force: true });
  fs.rmSync(tgz, { force: true });
  process.exit(pass ? 0 : 1);
})().catch((e) => { console.error('[smoke] failed:', e && e.message || e); process.exit(1); });
