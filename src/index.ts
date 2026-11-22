/**
 * The main entry point of the Nodine API SDK.
 *
 * A {@link Container} instance represents a browser-based Node.js runtime
 * ready to be used: mount files, spawn processes, and observe server/port
 * events — all executing locally inside the user's browser.
 *
 * @module
 */

export { Container, SDK_VERSION, DEFAULT_RUNTIME_URL } from './container.js';

export {
  BootFailedError,
  ContainerDestroyedError,
  EnvironmentUnsupportedError,
  FileSystemError,
  NodineError,
  NotYetImplementedError,
  SpawnError,
  UnsupportedFeatureError,
} from './errors/index.js';

export { Dirent } from './types/filesystem.js';

export type {
  BootOptions,
  BufferEncoding,
  ContainerEventName,
  ContainerProcess,
  DirectoryNode,
  ErrorListener,
  FileNode,
  FileSystemAPI,
  FileSystemTree,
  MkdirOptions,
  PortListener,
  PreviewMessage,
  PreviewMessageListener,
  ReadFileOptions,
  ReaddirOptions,
  RmOptions,
  ServerReadyListener,
  SpawnOptions,
  SymlinkNode,
  TerminalDimensions,
  WatchEventType,
  WatchListener,
  WatchOptions,
  Watcher,
  WriteFileOptions,
} from './types/index.js';

export type {
  BasePreviewMessage,
  ConsoleErrorMessage,
  UncaughtExceptionMessage,
  UnhandledRejectionMessage,
} from './types/events.js';
