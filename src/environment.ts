/**
 * Environment checks every embedding page must pass before the Nodine core
 * can boot. Mirrors the runtime's canonical execution profile (Chromium
 * desktop, HTTPS, cross-origin isolation, SharedArrayBuffer, WASM
 * threads/atomics, service workers — no fallback mode).
 */

export interface EnvironmentCheck {
  /** Short machine-readable check id. */
  id: string;
  /** Human-readable failure message. */
  message: string;
  /** Whether the check passed. */
  ok: boolean;
}

const inBrowser = typeof window !== 'undefined' && typeof document !== 'undefined';

/**
 * Runs all environment checks and returns them (passing and failing).
 *
 * Exported for diagnostics and tests; {@link Container.boot} calls it
 * internally and throws {@link EnvironmentUnsupportedError} on failure.
 */
export function checkEnvironment(): EnvironmentCheck[] {
  const checks: EnvironmentCheck[] = [];

  const hasWindow = typeof window !== 'undefined';
  const hasDocument = typeof document !== 'undefined';
  checks.push({
    id: 'browser-context',
    ok: hasWindow && hasDocument,
    message:
      'Nodine boots inside a browser page (window/document are unavailable — are you running in Node?)',
  });

  if (!inBrowser) return checks;

  checks.push({
    id: 'cross-origin-isolated',
    ok: window.crossOriginIsolated === true,
    message:
      'the page is not cross-origin isolated — serve COOP "same-origin" and COEP "require-corp" or "credentialless" headers from your application origin',
  });

  checks.push({
    id: 'shared-array-buffer',
    ok: typeof SharedArrayBuffer !== 'undefined',
    message: 'SharedArrayBuffer is unavailable (requires cross-origin isolation)',
  });

  checks.push({
    id: 'service-workers',
    ok: 'serviceWorker' in navigator,
    message:
      'Service Workers are unavailable (requires a secure context: https or localhost)',
  });

  checks.push({
    id: 'wasm',
    ok: typeof WebAssembly !== 'undefined',
    message: 'WebAssembly is unavailable',
  });

  // WASM threads/atomics: compile a module using shared memory + atomics.
  // Validated byte sequence: (module (memory 1 1 shared) (func (drop
  // (atomic.i32.load align=2 (i32.const 0))))). Compile-only probe.
  let wasmThreadsOk = false;
  try {
    const bytes = new Uint8Array([
      0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic + version
      0x01, 0x04, 0x01, 0x60, 0x00, 0x00,             // type: () -> ()
      0x03, 0x02, 0x01, 0x00,                         // func: one func, type 0
      0x05, 0x04, 0x01, 0x03, 0x01, 0x01,             // memory: shared, min 1, max 1
      0x0a, 0x0b, 0x01, 0x09, 0x00, 0x41, 0x00, 0xfe, 0x10, 0x02, 0x00, 0x1a, 0x0b, // code: atomic.i32.load; drop
    ]);
    new WebAssembly.Module(bytes);
    wasmThreadsOk = true;
  } catch {
    wasmThreadsOk = false;
  }
  checks.push({
    id: 'wasm-threads-atomics',
    ok: wasmThreadsOk,
    message: 'WebAssembly threads/atomics are unavailable (required by the Nodine kernel)',
  });

  return checks;
}

/** Returns only the failing checks (empty array = environment is suitable). */
export function environmentFailures(): EnvironmentCheck[] {
  return checkEnvironment().filter((c) => !c.ok);
}
