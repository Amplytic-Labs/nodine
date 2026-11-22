#!/usr/bin/env node
// build-runtime.mjs — regenerate the DERIVED artifacts of the runtime bundle
// in runtime/ from the vendored inputs in vendor/.
//
// Derived artifacts (checked in so a fresh clone is always publish-ready):
//   runtime/libsources.json    Node v26.13.0 lib sources (verbatim ids, no
//                              extensions) + the undici shim + the
//                              internal/deps dists + the B26 globalPaths graft
//   runtime/uvmap.json         libuv errno map from uv-errno-linux.txt
//   runtime/unpkg/{jsmd5,pako,noble}/   ESM trees the worker imports directly
//   runtime/npm-manifest.json  seed manifest (per-file fallback + index)
//   runtime/npm-seed-blob      CSB1 one-shot blob the supervisor prefers
//   runtime/runtime-version.txt content-hash stamp for cache-busting (?v=)
//
// Hand-written sources that ship VERBATIM (supervisor.mjs, runtime-worker.mjs,
// kernel-driver.js, kernel_core.wasm, sync-module-wrap.mjs, acorn-vendor.mjs,
// sw-opencontainer.js) are NOT touched by this script.
//
// Inputs (all committed under vendor/):
//   vendor/node-v26.13.0/lib   upstream Node lib sources (MIT)
//   vendor/deps/*.tgz          npm, @yarnpkg/shell, pako, @noble/hashes, js-md5
//   vendor/internal-deps/      acorn + minimatch dists (internal/deps ids)
//   vendor/undici-shim.js      browser-fetch shim for internal/deps/undici
//   vendor/uv-errno-linux.txt  libuv errno table
//   vendor/sh-stub.cjs         /bin/sh stub source
//   vendor/coreutils/coreutils.js  /usr/bin multi-call source
//
// Run: npm run build:runtime      (from the package root)

import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..');
const RUNTIME = path.join(ROOT, 'runtime');
const CACHE = path.join(ROOT, '.cache');
const VENDOR = path.join(ROOT, 'vendor');
const LIB = path.join(VENDOR, 'node-v26.13.0/lib');
const DEPS = path.join(VENDOR, 'deps');
const INT_DEPS = path.join(VENDOR, 'internal-deps');
const COREUTILS_SRC = path.join(VENDOR, 'coreutils/coreutils.js');

// Short cache names, mirroring the p2-007 server's unpack layout.
const TGZ_MAP = [
  ['npm-12.2.2.tgz', 'npm'],
  ['yarnpkg-shell-4.1.3.tgz', 'shell'],
  ['pako-2.1.0.tgz', 'pako'],
  ['noble-hashes-1.8.0.tgz', 'noble'],
  ['js-md5-0.8.3.tgz', 'jsmd5'],
];

const log = (m) => console.log('[build-runtime] ' + m);

// ---------------------------------------------------------------------------
// 0. unpack the vendored tarballs into .cache/ (gitignored), then re-apply
//    the $? patches to @yarnpkg/shell (a fresh unpack resets them).
// ---------------------------------------------------------------------------
function unpackDeps() {
  fs.mkdirSync(CACHE, { recursive: true });
  for (const [tgz, short] of TGZ_MAP) {
    const stamp = path.join(CACHE, short + '.stamp');
    if (fs.existsSync(stamp)) continue;
    const dst = path.join(CACHE, short);
    fs.rmSync(dst, { recursive: true, force: true });
    fs.mkdirSync(dst, { recursive: true });
    execFileSync('tar', ['-xzf', path.join(DEPS, tgz), '-C', dst], { stdio: 'pipe' });
    fs.writeFileSync(stamp, tgz + '\n');
    log(`unpacked ${tgz} -> .cache/${short}/package`);
  }
  // shell patch (idempotent, marker-guarded) — $? support
  const patchOut = execFileSync(process.execPath, [path.join(HERE, 'patch-yarn-shell.cjs')], { encoding: 'utf8' });
  for (const l of patchOut.trim().split('\n')) log(l);
  // shell dependency closure (the tarball ships WITHOUT its deps; /bin/sh
  // dies with "Cannot find module 'tslib'" otherwise). Network install of
  // pinned versions — only needed on rebuild, never to publish (the closure
  // is committed inside runtime/npm-seed-blob).
  const shellDeps = path.join(CACHE, 'shell/package/node_modules');
  if (!fs.existsSync(path.join(shellDeps, 'tslib'))) {
    log('shell dep closure missing (fresh .cache) — installing pinned closure…');
    execFileSync(process.execPath, [path.join(HERE, 'build-shell-deps.cjs')], { stdio: 'inherit' });
  }
  // hard verification: a complete shell cache has its runtime deps
  const late = path.join(CACHE, 'shell/package/node_modules/tslib/package.json');
  if (!fs.existsSync(late)) throw new Error('shell cache incomplete: tslib missing (bad unpack?)');
}

// ---------------------------------------------------------------------------
// 1. libsources.json — VERBATIM node lib, extensionless ids (the worker's
//    builtin lookup requests '_http_agent', not '_http_agent.js'), undici
//    shim, internal/deps dists, and the B26 globalPaths graft.
// ---------------------------------------------------------------------------
function buildLibsources() {
  const undiciShim = fs.readFileSync(path.join(VENDOR, 'undici-shim.js'), 'utf8');

  const walk = (dir, prefix = '') => {
    const out = [];
    for (const ent of fs.readdirSync(dir).sort()) {
      const p = path.join(dir, ent);
      if (fs.statSync(p).isDirectory()) out.push(...walk(p, `${prefix}${ent}/`));
      else if (ent.endsWith('.js') || ent.endsWith('.mjs')) out.push(prefix + ent.replace(/\.(js|mjs)$/, ''));
    }
    return out;
  };

  const ids = fs.existsSync(LIB) ? walk(LIB) : [];
  const sources = {};
  for (const id of ids) {
    sources[id] = id.endsWith('.mjs')
      ? fs.readFileSync(path.join(LIB, id + '.mjs'), 'utf8')
      : fs.readFileSync(path.join(LIB, id + '.js'), 'utf8');
  }

  // B26 (global root graft): the emulated node's npm installs globals to
  // /usr/lib/node_modules, but Node's _initPaths puts only the historical
  // 'lib/node' layout in Module.globalPaths. Every global-package resolver
  // in the ecosystem reads node:module.globalPaths expecting the npm root
  // among the entries. Inject the graft into the loader source itself.
  {
    const loaderId = 'internal/modules/cjs/loader';
    const anchor = '    Module._initPaths();';
    if (sources[loaderId] && sources[loaderId].includes(anchor) && !sources[loaderId].includes('/usr/lib/node_modules')) {
      sources[loaderId] = sources[loaderId].replace(anchor,
        anchor + '\n' +
        '    // B26 (runtime graft): the emulated node\'s npm global root.\n' +
        '    if (Array.isArray(Module.globalPaths) &&\n' +
        '        !Module.globalPaths.includes(\'/usr/lib/node_modules\')) {\n' +
        '      Module.globalPaths.push(\'/usr/lib/node_modules\');\n' +
        '    }');
      log('libsources: B26 globalPaths graft applied (loader)');
    }
  }

  // undici shim: Node's lib lazily requires it for global fetch; the shim
  // maps onto the browser's web APIs.
  ids.push('internal/deps/undici/undici');
  sources['internal/deps/undici/undici'] = undiciShim;

  // internal/deps dists (acorn, minimatch) — the vendored lib/ has no
  // internal/deps directory; the verbatim dist files live in
  // vendor/internal-deps mirroring the internal/deps/ id layout.
  if (fs.existsSync(INT_DEPS)) {
    for (const depId of walk(INT_DEPS, 'internal/deps/')) {
      if (sources[depId] !== undefined) continue; // undici shim wins
      const rel = depId.slice('internal/deps/'.length);
      ids.push(depId);
      sources[depId] = fs.readFileSync(path.join(INT_DEPS, rel + '.js'), 'utf8');
    }
  }

  return {
    nodeVersion: '26.13.0',
    tarballSha256: '263f639ecfa7a17b49438d76cdbf9aff73179e3c7cb789da69b900718427fa2e',
    count: ids.length,
    ids,
    sources,
  };
}

// ---------------------------------------------------------------------------
// 2. uvmap.json — libuv errno name/value/message table.
// ---------------------------------------------------------------------------
function buildUvmap() {
  const uvmap = fs.readFileSync(path.join(VENDOR, 'uv-errno-linux.txt'), 'utf8').trim().split('\n')
    .map((l) => {
      const m = l.match(/^(\S+) (-?\d+) (.*)$/);
      if (!m) throw new Error('bad uv line: ' + l);
      return { name: m[1], value: Number(m[2]), message: m[3] };
    });
  return uvmap;
}

// ---------------------------------------------------------------------------
// 3. unpkg/ tree — the worker imports pako / @noble/hashes / js-md5 ESM
//    directly by URL. @noble/hashes bare specifiers are rewritten to
//    relative URLs (relative to the STAGED file's location).
// ---------------------------------------------------------------------------
const rewriteNoble = (f) => {
  const src = fs.readFileSync(f, 'utf8');
  return src.replace(/(['"])@noble\/hashes\/([a-z0-9_/.]+?)\1/g, (m, q, rel) => {
    const withExt = /\.(js|mjs)$/.test(rel) ? rel : rel + '.js';
    return q + './' + withExt + q;
  });
};

function copyTree(srcDir, dstDir, transform) {
  fs.mkdirSync(dstDir, { recursive: true });
  let n = 0;
  const walk = (d, rd) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const s = path.join(d, e.name);
      const t = path.join(dstDir, rd ? `${rd}/${e.name}` : e.name);
      if (e.isDirectory()) { fs.mkdirSync(t, { recursive: true }); walk(s, rd ? `${rd}/${e.name}` : e.name); }
      else if (/\.(js|mjs)$/.test(e.name) && transform && s.includes('noble')) { fs.writeFileSync(t, transform(s)); n++; }
      else { fs.copyFileSync(s, t); n++; }
    }
  };
  walk(srcDir, '');
  return n;
}

// ---------------------------------------------------------------------------
// 4. npm seed — manifest + CSB1 blob. /usr tree (npm, shell, pako, noble),
//    exec stubs (/usr/bin/node, /bin/sh), coreutils as REAL /usr/bin
//    programs, and a small neutral demo workspace under /home/user.
// ---------------------------------------------------------------------------
function buildSeed() {
  const NODE_STUB = '#!/usr/bin/env node\n';
  const req = createRequire(import.meta.url);
  const { SH_STUB } = req(path.join(VENDOR, 'sh-stub.cjs'));

  const walkFiles = (pkgDir, vroot, out) => {
    const walk = (d, rel) => {
      for (const e of fs.readdirSync(d, { withFileTypes: true })) {
        const r = rel ? `${rel}/${e.name}` : e.name;
        if (e.isDirectory()) { out.push({ path: `${vroot}/${r}`, size: 0, mode: 0o755, type: 'dir' }); walk(path.join(d, e.name), r); }
        else if (e.isSymbolicLink()) out.push({ path: `${vroot}/${r}`, size: 0, mode: 0o777, type: 'symlink', target: fs.readlinkSync(path.join(d, e.name)) });
        else out.push({ path: `${vroot}/${r}`, size: fs.statSync(path.join(d, e.name)).size, mode: 0o644, type: 'file', file: path.join(d, e.name) });
      }
    };
    walk(pkgDir, '');
  };

  const files = [];
  for (const [dir, vroot] of [
    ['npm', '/usr/lib/node_modules/npm'],
    ['shell', '/usr/lib/node_modules/shell'],
    ['pako', '/usr/lib/node_modules/pako'],
    ['noble', '/usr/lib/node_modules/@noble/hashes'],
  ]) {
    const pkgDir = path.join(CACHE, dir, 'package');
    if (!fs.existsSync(pkgDir)) continue;
    const out = [];
    walkFiles(pkgDir, vroot, out);
    files.push(...out);
  }

  // noble alias
  files.push({ path: '/usr/lib/node_modules/noble-hashes', size: 0, mode: 0o777, type: 'symlink', target: '@noble/hashes' });

  // exec surface
  files.push(
    { path: '/usr/bin/npm', size: 0, mode: 0o777, type: 'symlink', target: '../lib/node_modules/npm/bin/npm-cli.js' },
    { path: '/usr/bin/npx', size: 0, mode: 0o777, type: 'symlink', target: '../lib/node_modules/npm/bin/npx-cli.js' },
    { path: '/usr/bin/node', size: Buffer.byteLength(NODE_STUB), mode: 0o755, type: 'file', stub: 'node' },
    { path: '/bin/sh', size: Buffer.byteLength(SH_STUB), mode: 0o755, type: 'file', stub: 'sh' },
    { path: '/home/user', size: 0, mode: 0o755, type: 'dir' },
  );

  // coreutils — real /usr/bin programs (multi-call source, argv[1] dispatch)
  const COREUTILS_COMMANDS = ['ls', 'cat', 'echo', 'head', 'tail', 'grep', 'wc', 'touch', 'mkdir', 'rmdir', 'rm', 'cp', 'mv', 'find', 'which', 'env', 'chmod', 'true', 'false'];
  const coreutilsSrc = fs.readFileSync(COREUTILS_SRC, 'utf8');
  for (const cmd of COREUTILS_COMMANDS) {
    files.push({ path: `/usr/bin/${cmd}`, size: Buffer.byteLength(coreutilsSrc), mode: 0o755, type: 'file', content: coreutilsSrc });
  }

  // neutral demo home
  files.push(
    { path: '/home/user/demo', size: 0, mode: 0o755, type: 'dir' },
    {
      path: '/home/user/demo/readme.md', size: 0, mode: 0o644, type: 'file',
      content: '# Welcome to Nodine\n\nA real Node.js runtime — in your browser.\nFiles, processes, packages. No server round-trips.\n',
    },
    {
      path: '/home/user/demo/notes.txt', size: 0, mode: 0o644, type: 'file',
      content: 'Try: mkdir projects && cd projects && pwd\nTry: grep "runtime" readme.md | wc -l\n',
    },
  );

  files.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));

  // CSB1 one-shot seed blob: one request instead of ~2200 per-file fetches.
  const blobFiles = [];
  const parts = [];
  let off = 0;
  for (const e of files) {
    if (e.type !== 'file') { blobFiles.push({ ...e }); continue; }
    let bytes;
    if (e.content !== undefined) bytes = Buffer.from(e.content);
    else if (e.stub === 'node') bytes = Buffer.from(NODE_STUB);
    else if (e.stub === 'sh') bytes = Buffer.from(SH_STUB);
    else bytes = fs.readFileSync(e.file);
    parts.push(bytes);
    const { file, stub, content, ...meta } = e;
    blobFiles.push({ ...meta, off, len: bytes.length });
    off += bytes.length;
  }
  const json = Buffer.from(JSON.stringify({ files: blobFiles, totalBytes: off, counts: {} }));
  const head = Buffer.alloc(8);
  head.write('CSB1', 0, 'ascii');
  head.writeUInt32LE(json.length, 4);
  const blob = Buffer.concat([head, json, ...parts]);
  return { files, blob };
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------
const t0 = Date.now();
unpackDeps();

const libsources = buildLibsources();
fs.writeFileSync(path.join(RUNTIME, 'libsources.json'), JSON.stringify(libsources));
log(`libsources.json: ${libsources.ids.length} files, ${(fs.statSync(path.join(RUNTIME, 'libsources.json')).size / 1048576).toFixed(2)} MB`);

fs.writeFileSync(path.join(RUNTIME, 'uvmap.json'), JSON.stringify(buildUvmap()));
log('uvmap.json written');

fs.rmSync(path.join(RUNTIME, 'unpkg'), { recursive: true, force: true });
const nJsmd5 = copyTree(path.join(CACHE, 'jsmd5'), path.join(RUNTIME, 'unpkg/jsmd5'));
const nPako = copyTree(path.join(CACHE, 'pako'), path.join(RUNTIME, 'unpkg/pako'));
const nNoble = copyTree(path.join(CACHE, 'noble'), path.join(RUNTIME, 'unpkg/noble'), rewriteNoble);
log(`unpkg: jsmd5 ${nJsmd5} files, pako ${nPako} files, noble ${nNoble} files (bare specifiers rewritten)`);

const { files, blob } = buildSeed();
fs.writeFileSync(path.join(RUNTIME, 'npm-seed-blob'), blob);
fs.writeFileSync(path.join(RUNTIME, 'npm-manifest.json'), JSON.stringify({ files, totalBytes: blob.length, counts: {} }));
log(`seed: ${files.length} entries, blob ${(blob.length / 1048576).toFixed(2)} MB -> runtime/npm-seed-blob`);

// content-hash version stamp: hosts append ?v=<stamp> to supervisor.mjs so
// any re-generated runtime busts both the module cache and Worker caches.
const hashFiles = ['supervisor.mjs', 'runtime-worker.mjs', 'kernel-driver.js'];
let stamp = '';
for (const f of hashFiles) {
  const p = path.join(RUNTIME, f);
  if (!fs.existsSync(p)) throw new Error('missing runtime source: ' + f);
  stamp += crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex').slice(0, 12);
}
fs.writeFileSync(path.join(RUNTIME, 'runtime-version.txt'), stamp + '\n');
log(`runtime-version.txt: ${stamp.slice(0, 12)}…`);

log(`DONE in ${Math.round((Date.now() - t0) / 1000)}s`);
