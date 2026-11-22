/** The set of events a container instance can emit. */
export type ContainerEventName =
  | 'server-ready'
  | 'port'
  | 'error'
  | 'preview-message';

/**
 * Emitted when a server started inside the container is ready to receive
 * traffic.
 *
 * @param port - The port the server is listening on.
 * @param url - An absolute URL under which the server can be reached from the
 * embedding page (suitable for iframe `src`).
 */
export type ServerReadyListener = (port: number, url: string) => void;

/**
 * Emitted when a port is opened or closed by some process inside the
 * container.
 *
 * @param port - The port that was opened or closed.
 * @param type - `'open'` or `'close'`.
 * @param url - An absolute URL for the port when open.
 */
export type PortListener = (
  port: number,
  type: 'open' | 'close',
  url: string,
) => void;

/**
 * Emitted when an internal runtime error occurs.
 *
 * @param error - An object with a human-readable `message`.
 */
export type ErrorListener = (error: { message: string }) => void;

/** Base fields shared by all preview messages. */
export interface BasePreviewMessage {
  /** Identifier of the preview that produced the message. */
  previewId: string;
  /** The port the preview is served on. */
  port: number;
  /** URL pathname inside the preview. */
  pathname: string;
  /** URL search string inside the preview. */
  search: string;
  /** URL hash fragment inside the preview. */
  hash: string;
}

/** An uncaught exception inside a preview page. */
export interface UncaughtExceptionMessage extends BasePreviewMessage {
  type: 'PREVIEW_UNCAUGHT_EXCEPTION';
  message: string;
  stack: string;
}

/** An unhandled promise rejection inside a preview page. */
export interface UnhandledRejectionMessage extends BasePreviewMessage {
  type: 'PREVIEW_UNHANDLED_REJECTION';
  message: string;
  /** Stack of the rejection reason when the browser provides one. */
  stack: string;
}

/** A `console.error` call inside a preview page. */
export interface ConsoleErrorMessage extends BasePreviewMessage {
  type: 'PREVIEW_CONSOLE_ERROR';
  message: string;
}

/**
 * A message forwarded from a preview iframe. Only emitted when the container
 * was booted with `forwardPreviewErrors` enabled.
 */
export type PreviewMessage =
  | UncaughtExceptionMessage
  | UnhandledRejectionMessage
  | ConsoleErrorMessage;

/**
 * Callback for `preview-message` events.
 */
export type PreviewMessageListener = (message: PreviewMessage) => void;
