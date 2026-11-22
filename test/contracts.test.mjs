/**
 * Contracts test: exercises the public API surface of the nodine package.
 *
 * Run with: npm test   (node --test, Node >= 18)
 *
 * These tests run in Node, so browser-only behavior (crossOriginIsolated,
 * service workers) is exercised where observable without a browser:
 * - Container.boot rejects with EnvironmentUnsupportedError outside a
 *   browser page (the browser-context check fails in Node).
 * - All type-level surface compiles via tsc (tsconfig.test.json).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  Container,
  NodineError,
  EnvironmentUnsupportedError,
  BootFailedError,
  NotYetImplementedError,
  UnsupportedFeatureError,
  ContainerDestroyedError,
  FileSystemError,
  SpawnError,
  SDK_VERSION,
} from '../dist/index.js';

test('SDK exports a Container class with the documented static surface', () => {
  assert.equal(typeof Container, 'function');
  assert.equal(typeof Container.boot, 'function');
  assert.equal(typeof Container.sdkVersion, 'string');
  assert.match(Container.sdkVersion, /^\d+\.\d+\.\d+/);
  assert.equal(SDK_VERSION, Container.sdkVersion);
});

test('Container.boot rejects outside a browser environment', async () => {
  await assert.rejects(
    Container.boot(),
    (err) => {
      assert.ok(err instanceof EnvironmentUnsupportedError);
      assert.ok(err instanceof NodineError);
      assert.equal(err.code, 'ERR_NODINE_UNSUPPORTED_ENVIRONMENT');
      // The first failing check in Node is the browser-context itself.
      assert.match(err.failures.join(' '), /window|document/);
      return true;
    },
  );
});

test('Container.boot forwards options and rejects (never hangs, never no-ops)', async () => {
  await assert.rejects(
    Container.boot({ coep: 'credentialless', workdirName: 'project' }),
    EnvironmentUnsupportedError,
  );
});

test('error classes carry distinct codes and readable messages', () => {
  const cases = [
    [new BootFailedError('x'), 'ERR_NODINE_BOOT_FAILED'],
    [new NotYetImplementedError('Container.mount'), 'ERR_NODINE_NOT_IMPLEMENTED'],
    [new UnsupportedFeatureError('auth', 'no'), 'ERR_NODINE_UNSUPPORTED_FEATURE'],
    [new ContainerDestroyedError(), 'ERR_NODINE_CONTAINER_DESTROYED'],
    [new FileSystemError('x'), 'ERR_NODINE_FS'],
    [new SpawnError('x'), 'ERR_NODINE_SPAWN'],
  ];
  for (const [err, code] of cases) {
    assert.ok(err instanceof NodineError, `${code} should extend NodineError`);
    assert.equal(err.code, code);
    assert.ok(err.message.length > 0, `${code} should have a message`);
  }
});

test('error messages avoid the previous product name', () => {
  // Naming decision: the SDK must not use any former product name anywhere.
  const samples = [
    new BootFailedError('core').message,
    new NotYetImplementedError('Container.spawn').message,
    new UnsupportedFeatureError('auth', 'reason').message,
    new ContainerDestroyedError().message,
  ];
  for (const msg of samples) {
    assert.ok(!/webcontainer/i.test(msg), `message leaked a former name: ${msg}`);
  }
});

test('NotYetImplementedError names the API member', () => {
  const err = new NotYetImplementedError('Container.spawn');
  assert.equal(err.api, 'Container.spawn');
});

test('process.interrupt() and container.baseEnv exist on the public surface (additive, batch 3)', async () => {
  // Static surface check: the additive APIs must exist on the prototypes.
  // (Behavioral interrupt testing needs the real runtime — covered by the
  // browser combination suite; here we pin the CONTRACT.)
  const { ContainerProcess } = await import('../dist/index.js').then((m) => ({ ContainerProcess: m.ContainerProcess ?? null }));
  // ContainerProcess is an interface (compile-time only); the runtime class
  // is internal. Assert via the public types file compiled output instead:
  const tsc = await import('node:fs').then((fs) =>
    fs.readFileSync(new URL('../dist/types/process.d.ts', import.meta.url), 'utf8'));
  assert.match(tsc, /interrupt\(\):\s*void/, 'interrupt() missing from public ContainerProcess type');
  assert.match(tsc, /SIGINT/, 'interrupt() doc must say SIGINT semantics');
  const tscContainer = await import('node:fs').then((fs) =>
    fs.readFileSync(new URL('../dist/container.d.ts', import.meta.url), 'utf8'));
  assert.match(tscContainer, /get baseEnv\(\)/, 'baseEnv getter missing from public Container');
});
