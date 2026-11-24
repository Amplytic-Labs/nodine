/**
 * Environment checks every embedding context must pass before the Nodine core
 * can boot. Mirrors the runtime's canonical execution profile (Chromium
 * desktop, HTTPS, cross-origin isolation, SharedArrayBuffer, WASM
 * threads/atomics — no fallback mode).
 *
 * Both browser pages AND Web Workers are supported: the runtime's kernel
 * driver and process workers are Worker-based themselves, so a Worker
 * embedding context (e.g. the VS Code web extension host) is fully capable.
 * Only `previewUrl`'s service-worker proxy needs a page context; it degrades
 * gracefully in Workers.
 */

export interface EnvironmentCheck {
  /** Short machine-readable check id. */
  id: string;
  /** Human-readable failure message. */
  message: string;
  /** Whether the check passed. */
  ok: boolean;
}

const inWindow = typeof window !== 'undefined' && typeof document !== 'undefined';
/** True inside any Web Worker (DedicatedWorker, SharedWorker, ServiceWorker). */
const inWorker =
  !inWindow &&
  typeof self !== 'undefined' &&
  typeof WorkerGlobalScope !== 'undefined' &&
  typeof importScripts === 'function';

/** The global scope that is guaranteed to exist in every supported context. */
const g: typeof globalThis & { crossOriginIsolated?: boolean } = globalThis;

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
    ok: (hasWindow && hasDocument) || inWorker,
    message:
      'Nodine boots inside a browser page or Worker (window/document are unavailable — are you running in Node?)',
  });

  if (!inWindow && !inWorker) return checks;

  checks.push({
    id: 'cross-origin-isolated',
    ok: g.crossOriginIsolated === true,
    message:
      'the embedding context is not cross-origin isolated — serve COOP "same-origin" and COEP "require-corp" or "credentialless" headers from your application origin',
  });

  checks.push({
    id: 'shared-array-buffer',
    ok: typeof SharedArrayBuffer !== 'undefined',
    message: 'SharedArrayBuffer is unavailable (requires cross-origin isolation)',
  });

  checks.push({
    id: 'service-workers',
    // Worker contexts have no navigator.serviceWorker; only previewUrl's
    // service-worker proxy needs it, and it degrades gracefully there.
    ok: inWorker || 'serviceWorker' in navigator,
    message:
      'Service Workers are unavailable (requires a page context and a secure context: https or localhost; previewUrl is unavailable in Workers)',
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
