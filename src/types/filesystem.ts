/**
 * A representation of a directory entry, mirroring the Node.js `fs.Dirent`
 * convention.
 *
 * @public
 */
export class Dirent {
  constructor(
    /** The name of the file or directory. */
    public readonly name: string | Uint8Array,
    private readonly _kind: 'file' | 'directory' | 'symlink',
  ) {}

  /** Whether this entry is a directory. */
  isDirectory(): boolean {
    return this._kind === 'directory';
  }

  /** Whether this entry is a regular file. */
  isFile(): boolean {
    return this._kind === 'file';
  }

  /** Whether this entry is a symbolic link. */
  isSymbolicLink(): boolean {
    return this._kind === 'symlink';
  }
}

/**
 * A handle returned by {@link FileSystemAPI.watch} that stops watching when
 * closed.
 *
 * @public
 */
export interface Watcher {
  /** Stop watching for changes. No further listener callbacks are invoked. */
  close(): void;
}

/**
 * A tree-like structure describing files, directories and symlinks to be
 * mounted into the container filesystem.
 *
 * @example
 * ```ts
 * const tree: FileSystemTree = {
 *   'package.json': { file: { contents: '{ "name": "demo" }' } },
 *   src: {
 *     directory: {
 *       'index.js': { file: { contents: 'console.log("hi")' } },
 *     },
 *   },
 * };
 * ```
 *
 * @public
 */
export interface FileSystemTree {
  /** Maps entry names to file, directory or symlink nodes. */
  [name: string]: FileNode | DirectoryNode | SymlinkNode;
}

/**
 * A file node in a {@link FileSystemTree}.
 *
 * @public
 */
export interface FileNode {
  /** Marks the node as a file. */
  file: {
    /** The file contents, as a UTF-8 string or raw bytes. */
    contents: string | Uint8Array;
  };
}

/**
 * A directory node in a {@link FileSystemTree}. Children are nested
 * {@link FileSystemTree} entries.
 *
 * @public
 */
export interface DirectoryNode {
  /** Marks the node as a directory and holds its children. */
  directory: FileSystemTree;
}

/**
 * A symbolic-link node in a {@link FileSystemTree}.
 *
 * @public
 */
export interface SymlinkNode {
  /** Marks the node as a symlink pointing at `target`. */
  symlink: string;
}

/** Encodings accepted by {@link FileSystemAPI.readFile} and file watching. */
export type BufferEncoding =
  | 'ascii'
  | 'utf8'
  | 'utf-8'
  | 'latin1'
  | 'binary'
  | 'hex'
  | 'base64'
  | 'base64url'
  | 'ucs2'
  | 'ucs-2'
  | 'utf16le';

/** Options accepted by {@link FileSystemAPI.readFile}. */
export interface ReadFileOptions {
  /**
   * Decode the file contents with this encoding. When omitted, the contents
   * are returned as raw bytes (`Uint8Array`).
   */
  encoding?: BufferEncoding | null;
}

/** Options accepted by {@link FileSystemAPI.writeFile}. */
export interface WriteFileOptions {
  /** Encoding used when `data` is a string. Defaults to UTF-8. */
  encoding?: BufferEncoding | null;
}

/** Options accepted by {@link FileSystemAPI.mkdir}. */
export interface MkdirOptions {
  /** When true, create any missing parent directories. Defaults to false. */
  recursive?: boolean;
}

/** Options accepted by {@link FileSystemAPI.readdir}. */
export interface ReaddirOptions {
  /**
   * Return entry names as raw bytes (`Uint8Array`) instead of strings.
   */
  encoding?: BufferEncoding | null;

  /**
   * When true, the return value is an array of {@link Dirent} objects
   * carrying file/directory/symlink kind information.
   */
  withFileTypes?: boolean;
}

/** Options accepted by {@link FileSystemAPI.rm}. */
export interface RmOptions {
  /** When true, missing paths are ignored instead of throwing. */
  force?: boolean;

  /** When true, remove directories and their contents recursively. */
  recursive?: boolean;
}

/** Options accepted by {@link FileSystemAPI.watch}. */
export interface WatchOptions {
  /** Encoding for the `filename` passed to the listener. Defaults to UTF-8. */
  encoding?: BufferEncoding | null;

  /**
   * Watch all subdirectories recursively. Only applies when a directory is
   * watched. Defaults to false.
   */
  recursive?: boolean;
}

/** Event names emitted by {@link FileSystemAPI.watch}. */
export type WatchEventType = 'rename' | 'change';

/**
 * Callback invoked on watched-path changes.
 *
 * @param event - `'rename'` when a file or directory is added/removed/renamed,
 *                `'change'` when content changes.
 * @param filename - The name of the changed file, when applicable.
 */
export type WatchListener = (
  event: WatchEventType,
  filename?: string | Uint8Array,
) => void;

/**
 * Interface to interact directly with the container filesystem. Modeled after
 * Node.js `fs.promises`.
 *
 * @public
 */
export interface FileSystemAPI {
  /**
   * Create a directory. Throws if the directory already exists, unless
   * `options.recursive` is true.
   */
  mkdir(path: string, options?: MkdirOptions): Promise<void>;

  /**
   * Read the entries of a directory.
   *
   * @returns Entry names, or {@link Dirent} objects when
   * `options.withFileTypes` is true.
   */
  readdir(
    path: string,
    options: ReaddirOptions & { withFileTypes: true },
  ): Promise<Dirent[]>;
  readdir(
    path: string,
    options?: ReaddirOptions,
  ): Promise<string[] | Uint8Array[]>;

  /**
   * Read a file.
   *
   * @returns Raw bytes, or a string when `options.encoding` is set.
   */
  readFile(path: string, options?: ReadFileOptions): Promise<Uint8Array | string>;

  /**
   * Write a file. Creates the file if it does not exist; overwrites it if it
   * does.
   */
  writeFile(
    path: string,
    data: string | Uint8Array,
    options?: WriteFileOptions,
  ): Promise<void>;

  /**
   * Rename a file or directory. The parent directories of `newPath` must
   * exist.
   */
  rename(oldPath: string, newPath: string): Promise<void>;

  /**
   * Delete a file, or a directory when `options.recursive` is true.
   */
  rm(path: string, options?: RmOptions): Promise<void>;

  /**
   * Watch a file or directory for changes. Returns a {@link Watcher} that
   * stops watching when closed.
   */
  watch(
    path: string,
    listener: WatchListener,
  ): Watcher;
  watch(
    path: string,
    options: WatchOptions,
    listener: WatchListener,
  ): Watcher;
}
