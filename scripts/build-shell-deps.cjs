#!/usr/bin/env node
'use strict';
// build-shell-deps.cjs — vendor @yarnpkg/shell's UNBUNDLED dependency closure
// into .cache/shell/package/node_modules — /bin/sh requires the vendored
// shell, and the @yarnpkg/shell tarball ships without its deps (tslib,
// @yarnpkg/fslib, @yarnpkg/parsers, chalk@4, clipanion, cross-spawn,
// fast-glob, micromatch + transitive), so /bin/sh would die instantly with
// "Cannot find module 'tslib'".
//
// Versions are PINNED for reproducibility. The build's manifest walk picks
// the tree up automatically; .cache is only wiped when vendor/deps/*.tgz
// changes. Requires network access (registry.npmjs.org) — the resulting
// closure is committed inside runtime/npm-seed-blob, so publishing never
// needs this step.
//
// Run: node scripts/build-shell-deps.cjs   (from the package root)
const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

const SHELL_PKG = path.resolve(__dirname, '../.cache/shell/package');
if (!fs.existsSync(path.join(SHELL_PKG, 'package.json'))) {
  console.error('shell package not unpacked yet — run scripts/build-runtime.mjs first');
  process.exit(1);
}

// Exact closure of @yarnpkg/shell@4.1.3 production deps (npm ls, --omit=dev).
const PINS = [
  '@yarnpkg/fslib@3.1.5',
  '@yarnpkg/parsers@3.1.0',
  'chalk@4.1.2',
  'clipanion@4.0.0-rc.4',
  'cross-spawn@7.0.6',
  'fast-glob@3.3.3',
  'micromatch@4.0.8',
  'tslib@2.9.1',
];

const t0 = Date.now();
// Install into a SCRATCH prefix (NOT the shell package dir): the shell's own
// package.json carries private devDependencies (@yarnpkg/monorepo@^0.0.0 —
// 404s) that npm insists on resolving even with --omit=dev. The pinned
// production closure pulls the full transitive graph by itself.
const SCRATCH = fs.mkdtempSync(path.join(os.tmpdir(), 'shell-deps-'));
try {
  console.log('[shell-deps] installing pinned closure (scratch:', SCRATCH + ')');
  execFileSync('npm', [
    'install', '--prefix', SCRATCH, '--omit=dev', '--ignore-scripts',
    '--no-audit', '--no-fund', '--no-package-lock', ...PINS,
  ], { stdio: 'inherit' });
  // Move the tree in (cp, not rename: /tmp may be another filesystem).
  // verbatimSymlinks: .bin/ and package-internal symlinks must survive AS
  // symlinks — the manifest walk records them as type:'symlink'.
  const dst = path.join(SHELL_PKG, 'node_modules');
  fs.rmSync(dst, { recursive: true, force: true });
  fs.cpSync(path.join(SCRATCH, 'node_modules'), dst, { recursive: true, verbatimSymlinks: true });
} finally {
  fs.rmSync(SCRATCH, { recursive: true, force: true });
}

const nm = path.join(SHELL_PKG, 'node_modules');
let files = 0, bytes = 0, symlinks = 0;
(function walk(d) {
  for (const ent of fs.readdirSync(d)) {
    const p = path.join(d, ent);
    const st = fs.lstatSync(p);
    if (st.isSymbolicLink()) symlinks++;
    else if (st.isDirectory()) walk(p);
    else { files++; bytes += st.size; }
  }
})(nm);
console.log(`[shell-deps] done in ${Date.now() - t0} ms: ${files} files, ${symlinks} symlinks, ${(bytes / 1048576).toFixed(1)} MiB under node_modules/`);
