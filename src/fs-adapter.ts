/**
 * FileSystemAPI implementation over the runtime supervisor's kernel-backed
 * helpers (kstat/kreaddir/kreadWhole/kwriteFile/…). No runtime changes: the
 * adapter only calls public supervisor methods.
 */

import type {
  Dirent as DirentType,
  FileSystemAPI,
  Watcher,
} from './types/index.js';
import { Dirent } from './types/filesystem.js';
import { FileSystemError, NotYetImplementedError } from './errors/index.js';
import type { RuntimeSupervisor } from './runtime-adapter.js';

/** Union of readdir option shapes (mirrors the FileSystemAPI overloads). */
type ReaddirOptionsUnion = { withFileTypes?: boolean; encoding?: string };

const dec = new TextDecoder();

/** Text encodings Node supports that we can decode locally. */
function decodeBytes(u8: Uint8Array, encoding: string): string {
  switch (encoding) {
    case 'utf8':
    case 'utf-8':
    case 'ascii':
    case 'latin1':
    case 'binary':
    case 'ucs2':
    case 'ucs-2':
    case 'utf16le':
      // The runtime emits UTF-8; the aliases map onto UTF-8 decoding in this
      // implementation (documented limitation for non-UTF-8 payloads).
      return dec.decode(u8);
    case 'hex':
      return Array.from(u8, (b) => b.toString(16).padStart(2, '0')).join('');
    case 'base64': {
      let bin = '';
      const CHUNK = 0x8000;
      for (let i = 0; i < u8.length; i += CHUNK) {
        bin += String.fromCharCode(...u8.subarray(i, i + CHUNK));
      }
      return btoa(bin);
    }
    case 'base64url': {
      let bin = '';
      const CHUNK = 0x8000;
      for (let i = 0; i < u8.length; i += CHUNK) {
        bin += String.fromCharCode(...u8.subarray(i, i + CHUNK));
      }
      return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    }
    default:
      throw new FileSystemError(`unsupported encoding: ${encoding}`);
  }
}

function toUint8(data: string | Uint8Array, encoding: string | undefined): Uint8Array {
  if (typeof data !== 'string') return data;
  const enc = new TextEncoder(); // UTF-8 (aliases documented as UTF-8)
  if (encoding === 'hex') {
    const clean = data.replace(/[^0-9a-fA-F]/g, '');
    const out = new Uint8Array(clean.length >> 1);
    for (let i = 0; i < out.length; i++) {
      out[i] = parseInt(clean.substr(i * 2, 2), 16);
    }
    return out;
  }
  if (encoding === 'base64' || encoding === 'base64url') {
    const b64 = encoding === 'base64url'
      ? data.replace(/-/g, '+').replace(/_/g, '/')
      : data;
    const bin = atob(b64);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }
  return enc.encode(data);
}

function errnoMessage(status: number, path: string, op: string): string {
  switch (status) {
    case -2: return `${op}: ${path}: no such file or directory (ENOENT)`;
    case -13: return `${op}: ${path}: permission denied (EACCES)`;
    case -17: return `${op}: ${path}: file already exists (EEXIST)`;
    case -20: return `${op}: ${path}: not a directory (ENOTDIR)`;
    case -21: return `${op}: ${path}: is a directory (EISDIR)`;
    case -39: return `${op}: ${path}: directory not empty (ENOTEMPTY)`;
    default: return `${op}: ${path}: errno ${status}`;
  }
}

export function createFileSystem(sup: RuntimeSupervisor, resolve: (p: string) => string): FileSystemAPI {
  async function readdirImpl(
    path: string,
    options?: ReaddirOptionsUnion,
  ): Promise<DirentType[] | string[]> {
    const abs = resolve(path);
    const st = await sup.klstat(abs);
    if (!st.ok) throw new FileSystemError(errnoMessage(st.status ?? -2, path, 'readdir'));
    if (!st.isDir) throw new FileSystemError(errnoMessage(-20, path, 'readdir'));
    const names = await sup.kreaddir(abs);
    if (names === null) throw new FileSystemError(errnoMessage(-5, path, 'readdir'));
    if (options?.withFileTypes) {
      const out: DirentType[] = [];
      for (const name of names) {
        const est = await sup.klstat(abs === '/' ? '/' + name : abs + '/' + name);
        out.push(new Dirent(
          options.encoding && options.encoding !== 'utf-8' && options.encoding !== 'utf8'
            ? new TextEncoder().encode(name)
            : name,
          est.ok ? (est.isDir ? 'directory' : est.isSymlink ? 'symlink' : 'file') : 'file',
        ));
      }
      return out;
    }
    return names;
  }

  const fs: FileSystemAPI = {
    async mkdir(path, options) {
      const abs = resolve(path);
      if (options?.recursive) {
        await sup.mkdirP(abs);
        return;
      }
      const st = await sup.kmkdirOne(abs, 0o777);
      if (st !== 0) throw new FileSystemError(errnoMessage(st, path, 'mkdir'));
    },

    readdir: readdirImpl as FileSystemAPI['readdir'],

    async readFile(path, options) {
      const abs = resolve(path);
      const st = await sup.kstat(abs);
      if (!st.ok) throw new FileSystemError(errnoMessage(st.status ?? -2, path, 'readFile'));
      if (st.isDir) throw new FileSystemError(errnoMessage(-21, path, 'readFile'));
      const bytes = await sup.kreadWhole(abs);
      if (bytes === null) throw new FileSystemError(errnoMessage(-5, path, 'readFile'));
      const enc = options?.encoding ?? null;
      if (enc) return decodeBytes(bytes, enc);
      return bytes;
    },

    async writeFile(path, data, options) {
      const abs = resolve(path);
      await sup.kwriteFile(abs, toUint8(data, options?.encoding ?? 'utf-8'), 0o644);
    },

    async rename(oldPath, newPath) {
      // The frozen kernel ABI (v1.0.2) has no rename op: implement as
      // copy + unlink. Directories are not supported (kernel limitation).
      const absOld = resolve(oldPath);
      const absNew = resolve(newPath);
      const st = await sup.klstat(absOld);
      if (!st.ok) throw new FileSystemError(errnoMessage(st.status ?? -2, oldPath, 'rename'));
      if (st.isDir) throw new FileSystemError('rename: directory rename is not supported by the Nodine kernel ABI');
      const bytes = await sup.kreadWhole(absOld);
      if (bytes === null) throw new FileSystemError(errnoMessage(-5, oldPath, 'rename'));
      await sup.kwriteFile(absNew, bytes, st.mode ?? 0o644);
      const rc = await sup.kunlink(absOld);
      if (rc !== 0) throw new FileSystemError(errnoMessage(rc, oldPath, 'rename'));
    },

    async rm(path, options) {
      const abs = resolve(path);
      const st = await sup.klstat(abs);
      if (!st.ok) {
        if (options?.force) return;
        throw new FileSystemError(errnoMessage(st.status ?? -2, path, 'rm'));
      }
      if (st.isDir) {
        if (!options?.recursive) {
          throw new FileSystemError(`rm: ${path}: is a directory (EISDIR)`);
        }
        await rmRecursive(abs, sup);
        return;
      }
      const rc = await sup.kunlink(abs);
      if (rc !== 0) throw new FileSystemError(errnoMessage(rc, path, 'rm'));
    },

    watch(path: string, ...rest: unknown[]): Watcher {
      void path;
      void rest;
      throw new NotYetImplementedError('container.fs.watch (kernel change journal is milestone M4)');
    },
  };
  return fs;
}

async function rmRecursive(abs: string, sup: RuntimeSupervisor): Promise<void> {
  const st = await sup.klstat(abs);
  if (!st.ok) return;
  if (!st.isDir) {
    await sup.kunlink(abs);
    return;
  }
  const names = await sup.kreaddir(abs);
  if (names) {
    for (const name of names) {
      await rmRecursive(abs === '/' ? '/' + name : abs + '/' + name, sup);
    }
  }
  const rc = await sup.krmdir(abs);
  if (rc !== 0) throw new FileSystemError(errnoMessage(rc, abs, 'rmdir'));
}

export { decodeBytes, toUint8 };
