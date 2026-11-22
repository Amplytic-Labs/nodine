/**
 * Error base class for all errors thrown by the Nodine API SDK.
 *
 * @public
 */
export class NodineError extends Error {
  constructor(
    message: string,
    /** Machine-readable error code, e.g. `'ERR_NODINE_BOOT_FAILED'`. */
    public readonly code: string,
  ) {
    super(message);
    this.name = 'NodineError';
  }
}

/**
 * Thrown when {@link Container.boot} is called in a browser environment that
 * cannot host the Nodine runtime.
 *
 * Typical causes: the page is not cross-origin isolated (COOP/COEP headers
 * missing), `SharedArrayBuffer` is unavailable, service workers are
 * unavailable (insecure context), or WASM threads/atomics are unsupported.
 */
export class EnvironmentUnsupportedError extends NodineError {
  /** The list of failed environment checks. */
  readonly failures: string[];

  constructor(failures: string[]) {
    super(
      'This browser environment cannot run the Nodine runtime: ' +
        failures.join('; '),
      'ERR_NODINE_UNSUPPORTED_ENVIRONMENT',
    );
    this.name = 'EnvironmentUnsupportedError';
    this.failures = failures;
  }
}

/**
 * Thrown when {@link Container.boot} fails to load or initialize the Nodine
 * core runtime.
 */
export class BootFailedError extends NodineError {
  constructor(message: string) {
    super(message, 'ERR_NODINE_BOOT_FAILED');
    this.name = 'BootFailedError';
  }
}

/**
 * Thrown when calling an API that is defined for compatibility but not yet
 * implemented by the Nodine runtime.
 *
 * The SDK never silently no-ops: unimplemented functionality fails loudly.
 */
export class NotYetImplementedError extends NodineError {
  /** The API member that was called. */
  readonly api: string;

  constructor(api: string) {
    super(
      `${api} is defined in the Nodine API contract but is not implemented ` +
        `by the Nodine runtime yet (see the implementation-status report).`,
      'ERR_NODINE_NOT_IMPLEMENTED',
    );
    this.name = 'NotYetImplementedError';
    this.api = api;
  }
}

/**
 * Thrown when calling an API that Nodine does not support and does not plan
 * to support (for example authentication flows belonging to foreign
 * platforms).
 */
export class UnsupportedFeatureError extends NodineError {
  /** The API member that was called. */
  readonly api: string;

  constructor(api: string, reason: string) {
    super(
      `${api} is not supported by Nodine: ${reason}`,
      'ERR_NODINE_UNSUPPORTED_FEATURE',
    );
    this.name = 'UnsupportedFeatureError';
    this.api = api;
  }
}

/** Thrown when an operation is attempted on a torn-down container. */
export class ContainerDestroyedError extends NodineError {
  constructor() {
    super(
      'This container instance was torn down and can no longer be used. ' +
        'Call Container.boot() to create a new instance.',
      'ERR_NODINE_CONTAINER_DESTROYED',
    );
    this.name = 'ContainerDestroyedError';
  }
}

/** Thrown when filesystem operations fail (missing paths, type errors, …). */
export class FileSystemError extends NodineError {
  constructor(message: string) {
    super(message, 'ERR_NODINE_FS');
    this.name = 'FileSystemError';
  }
}

/** Thrown when a process cannot be spawned (command not found, …). */
export class SpawnError extends NodineError {
  constructor(message: string) {
    super(message, 'ERR_NODINE_SPAWN');
    this.name = 'SpawnError';
  }
}
