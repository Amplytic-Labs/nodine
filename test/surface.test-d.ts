/**
 * Type-level smoke checks for the public API surface: assigning mock
 * implementations against every declared interface verifies the contracts are
 * structurally complete and usable by consumers. Compiled by
 * tsconfig.test.json (npm run test:types); no runtime behavior.
 */

import type {
  BootOptions,
  BufferEncoding,
  ContainerEventName,
  ContainerProcess,
  Dirent,
  FileSystemAPI,
  FileSystemTree,
  PreviewMessage,
  SpawnOptions,
  TerminalDimensions,
  WatchListener,
  WatchOptions,
  Watcher,
} from '../src/index.js';

// BootOptions accepts the documented shapes.
const bootOpts: BootOptions = {
  coep: 'credentialless',
  workdirName: 'project',
  forwardPreviewErrors: 'exceptions-only',
};

// FileSystemTree accepts files, directories and symlinks.
const tree: FileSystemTree = {
  'package.json': { file: { contents: '{"name":"demo"}' } },
  'logo.png': { file: { contents: new Uint8Array([1, 2, 3]) } },
  src: { directory: { 'index.js': { file: { contents: 'console.log(1)' } } } },
  link: { symlink: './package.json' },
};

// A complete mock of the fs contract proves the interface is implementable.
const watcher: Watcher = { close() {} };

// readdir with declared overloads, assignable to the overloaded interface member.
function readdirImpl(
  path: string,
  options: { withFileTypes: true },
): Promise<Dirent[]>;
function readdirImpl(
  path: string,
  options?: { withFileTypes?: boolean },
): Promise<string[]>;
function readdirImpl(
  _path: string,
  options?: { withFileTypes?: boolean },
): Promise<Dirent[] | string[]> {
  if (options && options.withFileTypes) {
    const d: Dirent = {
      name: 'a',
      isDirectory: () => false,
      isFile: () => true,
      isSymbolicLink: () => false,
    } as unknown as Dirent;
    return Promise.resolve([d]);
  }
  return Promise.resolve(['a']);
}

const fs: FileSystemAPI = {
  async mkdir(_path, _options) {},
  readdir: readdirImpl,
  async readFile(_path, options) {
    return options && options.encoding ? 'contents' : new Uint8Array(0);
  },
  async writeFile(_path, _data, _options) {},
  async rename(_oldPath, _newPath) {},
  async rm(_path, _options) {},
  watch(
    _path: string,
    optionsOrListener: WatchOptions | WatchListener,
    maybeListener?: WatchListener,
  ): Watcher {
    const listener =
      typeof optionsOrListener === 'function'
        ? optionsOrListener
        : (maybeListener as WatchListener);
    listener('change', 'x');
    return watcher;
  },
};

// Process contract: streams, exit promise, kill, interrupt, resize.
const dims: TerminalDimensions = { cols: 80, rows: 24 };
const proc: ContainerProcess = {
  exit: Promise.resolve(0),
  input: new WritableStream<string>(),
  output: new ReadableStream<string>(),
  kill() {},
  interrupt() {},
  resize(_d) {},
};

// Event payload union discriminates on `type`.
const msg: PreviewMessage = {
  type: 'PREVIEW_UNCAUGHT_EXCEPTION',
  previewId: 'p1',
  port: 3000,
  pathname: '/',
  search: '',
  hash: '',
  message: 'boom',
  stack: '',
};

// SpawnOptions covers the documented fields.
const spawnOpts: SpawnOptions = {
  cwd: '.',
  env: { NODE_ENV: 'production', PORT: 3000, VERBOSE: true },
  output: false,
  terminal: dims,
};

const eventNames: ContainerEventName[] = [
  'server-ready',
  'port',
  'error',
  'preview-message',
];

const encodings: BufferEncoding[] = ['utf-8', 'base64', 'hex'];

// Touch all bindings so noUnusedLocals keeps them honest.
void [bootOpts, tree, fs, proc, msg, spawnOpts, eventNames, encodings];
