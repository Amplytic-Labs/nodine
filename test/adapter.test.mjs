/**
 * Unit tests for the M1.2 adapter layer: HeadlessTermSink, fs-adapter over
 * a mocked supervisor, mount walker, and error paths. These run in Node with
 * a mocked RuntimeSupervisor — the real-runtime E2E is a separate harness.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { HeadlessTermSink, assertRuntimeSurface } from '../dist/runtime-adapter.js';
import { createFileSystem } from '../dist/fs-adapter.js';
import { Dirent, FileSystemError, NotYetImplementedError } from '../dist/index.js';

// -- HeadlessTermSink -------------------------------------------------------

test('HeadlessTermSink collects write/meta and fans out to taps', () => {
  const sink = new HeadlessTermSink();
  const seen = [];
  const off = sink.onTap((c) => seen.push(c));
  sink.write('hello ');
  sink.meta('[status]\n');
  sink.write('world');
  off();
  sink.write(' not-seen');
  assert.equal(sink.dump(), 'hello [status]\nworld not-seen');
  assert.deepEqual(seen, ['hello ', '[status]\n', 'world']);
});

test('HeadlessTermSink tolerates throwing subscribers', () => {
  const sink = new HeadlessTermSink();
  sink.onTap(() => { throw new Error('boom'); });
  sink.write('still works');
  assert.equal(sink.dump(), 'still works');
});

// -- assertRuntimeSurface ---------------------------------------------------

test('assertRuntimeSurface rejects a supervisor missing members', () => {
  assert.throws(
    () => assertRuntimeSurface({ boot() {} }),
    /missing required members: .*seed/,
  );
});

// -- fs-adapter over a mocked supervisor ------------------------------------

/** In-memory VFS mock implementing the supervisor fs surface. */
function makeMockSup() {
  const files = new Map();
  const dirs = new Set(['/', '/home', '/home/user', '/home/user/project']);
  const symlinks = new Map();
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const sup = {
    files, dirs, symlinks,
    baseEnv: { HOME: '/home/user', PATH: '/usr/bin:/bin' },
    stopped: false,
    status: { kernel: {}, procs: {}, cwd: {}, exit: {}, seed: {} },
    setKernel() {},
    boot: async () => {},
    seed: async () => {},
    spawnProcess: async () => null,
    signalTree() {},
    shutdown: async () => { sup.stopped = true; },
    initPreviewSW() {},
    onPreviewEvent: null,
    async kstat(p) {
      if (files.has(p)) return { ok: true, mode: 0o100644, size: files.get(p).length, isFile: true, isDir: false };
      if (dirs.has(p)) return { ok: true, mode: 0o040755, size: 0, isFile: false, isDir: true };
      return { ok: false, status: -2 };
    },
    async klstat(p) {
      if (symlinks.has(p)) return { ok: true, mode: 0o120777, size: 0, isFile: false, isDir: false, isSymlink: true };
      return sup.kstat(p);
    },
    async mkdirP(d) {
      const parts = d.split('/').filter(Boolean);
      let cur = '';
      for (const part of parts) { cur += '/' + part; dirs.add(cur); }
    },
    async kmkdirOne(p) {
      if (dirs.has(p) || files.has(p)) return -17;
      dirs.add(p);
      return 0;
    },
    async kunlink(p) { return files.delete(p) || symlinks.delete(p) ? 0 : -2; },
    async krmdir(p) { return dirs.delete(p) ? 0 : -2; },
    async ksymlink(target, link) { symlinks.set(link, target); },
    async kwriteFile(p, u8) {
      await sup.mkdirP(p.slice(0, p.lastIndexOf('/')) || '/');
      files.set(p, u8);
    },
    async kreadWhole(p) { return files.get(p) ?? null; },
    async kreaddir(p) {
      if (!dirs.has(p)) return null;
      const names = new Set();
      const prefix = p === '/' ? '/' : p + '/';
      for (const f of files.keys()) {
        if (f.startsWith(prefix) && !f.slice(prefix.length).includes('/')) names.add(f.slice(prefix.length));
      }
      for (const d of dirs) {
        if (d.startsWith(prefix) && !d.slice(prefix.length).includes('/') && d !== prefix) names.add(d.slice(prefix.length));
      }
      return [...names];
    },
    async resolveSymlinks(p) { return symlinks.get(p) ?? p; },
    abs(cwd, p) {
      if (p.startsWith('/')) return sup.normalizePath(p);
      return sup.normalizePath(cwd + '/' + p);
    },
    normalizePath(p) {
      const abs = p.startsWith('/');
      const out = [];
      for (const part of p.split('/')) {
        if (!part || part === '.') continue;
        if (part === '..') { if (out.length) out.pop(); continue; }
        out.push(part);
      }
      const joined = out.join('/');
      return abs ? '/' + joined : (joined || '.');
    },
    parentOf(p) { const i = p.lastIndexOf('/'); return i <= 0 ? '/' : p.slice(0, i); },
  };
  return sup;
}

const WORKDIR = '/home/user/project';
function normalizePath(p) {
  const abs = p.startsWith('/');
  const out = [];
  for (const part of p.split('/')) {
    if (!part || part === '.') continue;
    if (part === '..') { if (out.length) out.pop(); continue; }
    out.push(part);
  }
  const joined = out.join('/');
  return abs ? '/' + joined : (joined || '.');
}
const resolve = (p) => p.startsWith('/') ? normalizePath(p) : normalizePath(WORKDIR + '/' + p);

test('fs.mkdir / writeFile / readFile round-trip (bytes and utf8)', async () => {
  const sup = makeMockSup();
  const fs = createFileSystem(sup, resolve);
  await fs.mkdir('sub');
  await fs.writeFile('sub/a.txt', 'héllo', { encoding: 'utf-8' });
  const asText = await fs.readFile('sub/a.txt', { encoding: 'utf-8' });
  assert.equal(asText, 'héllo');
  const asBytes = await fs.readFile('sub/a.txt');
  assert.ok(asBytes instanceof Uint8Array);
  assert.equal(asBytes.length, 6); // é is 2 bytes in UTF-8
});

test('fs.mkdir without recursive throws EEXIST; with recursive creates parents', async () => {
  const sup = makeMockSup();
  const fs = createFileSystem(sup, resolve);
  await fs.mkdir('a');            // first creation succeeds
  await assert.rejects(fs.mkdir('a'), /EEXIST/); // second fails
  await fs.mkdir('x/y/z', { recursive: true });
  await fs.writeFile('x/y/z/f', '1');
  assert.equal(await fs.readFile('x/y/z/f', { encoding: 'utf8' }), '1');
});

test('fs.readdir lists entries; withFileTypes returns Dirent kinds', async () => {
  const sup = makeMockSup();
  const fs = createFileSystem(sup, resolve);
  await fs.writeFile('f1.js', '1');
  await fs.mkdir('d1', { recursive: true });
  await fs.mkdir('d1/d2', { recursive: true });
  const names = await fs.readdir('.');
  assert.ok(names.includes('f1.js') && names.includes('d1'));
  const entries = await fs.readdir('.', { withFileTypes: true });
  const f1 = entries.find((e) => e.name === 'f1.js');
  const d1 = entries.find((e) => e.name === 'd1');
  assert.ok(f1 instanceof Dirent && f1.isFile());
  assert.ok(d1.isDirectory());
  await assert.rejects(fs.readdir('nope'), /ENOENT/);
});

test('fs.rm removes files; recursive removes trees; force tolerates missing', async () => {
  const sup = makeMockSup();
  const fs = createFileSystem(sup, resolve);
  await fs.writeFile('gone.txt', 'x');
  await fs.rm('gone.txt');
  await fs.rm('gone.txt', { force: true }); // no throw
  await assert.rejects(fs.rm('gone.txt'), /ENOENT/);
  await fs.mkdir('t/a/b', { recursive: true });
  await fs.writeFile('t/a/b/c.txt', 'x');
  await assert.rejects(fs.rm('t'), /is a directory/);
  await fs.rm('t', { recursive: true });
  await assert.rejects(fs.readdir('t'), /ENOENT/);
});

test('fs.rename copies + unlinks (kernel ABI has no rename op)', async () => {
  const sup = makeMockSup();
  const fs = createFileSystem(sup, resolve);
  await fs.writeFile('old.txt', 'payload');
  await fs.rename('old.txt', 'new.txt');
  assert.equal(await fs.readFile('new.txt', { encoding: 'utf-8' }), 'payload');
  await assert.rejects(fs.readFile('old.txt'), /ENOENT/);
});

test('fs.readFile on a directory throws EISDIR; readFile ENOENT path', async () => {
  const sup = makeMockSup();
  const fs = createFileSystem(sup, resolve);
  await fs.mkdir('d', { recursive: true });
  await assert.rejects(fs.readFile('d'), /is a directory/);
  await assert.rejects(fs.readFile('missing.txt'), /ENOENT/);
});

test('fs.watch throws NotYetImplementedError (M4)', async () => {
  const sup = makeMockSup();
  const fs = createFileSystem(sup, resolve);
  assert.throws(() => fs.watch('.', () => {}), NotYetImplementedError);
  assert.throws(() => fs.watch('.', {}, () => {}), NotYetImplementedError);
});

test('hex and base64 encodings round-trip', async () => {
  const sup = makeMockSup();
  const fs = createFileSystem(sup, resolve);
  await fs.writeFile('h.txt', 'ff00a1', { encoding: 'hex' });
  const hexBytes = await fs.readFile('h.txt');
  assert.deepEqual([...hexBytes], [0xff, 0x00, 0xa1]);
  await fs.writeFile('b.txt', 'AQIDBA==', { encoding: 'base64' });
  const b64Bytes = await fs.readFile('b.txt');
  assert.deepEqual([...b64Bytes], [1, 2, 3, 4]);
  const asB64 = await fs.readFile('b.txt', { encoding: 'base64' });
  assert.equal(asB64, 'AQIDBA==');
});
