//! The VFS instance and its operations (spec §1–§9).
//!
//! Every op is all-checks-before-first-mutation (ruling via F/H, incl. the
//! rename `names_bytes` pre-check — AV-19), resource deltas are pre-flight
//! charged (ENOSPC/EFBIG never rely on allocation failure), and the observer
//! seam fires once per committed mutation. Times are injected per op
//! (`now_ns`) so tests are deterministic; `umask` is a caller parameter
//! (per-process state lives in kernel-core; default `DEFAULT_UMASK`).
//!
//! Known interim divergence (P2-005-008 scope, differential-tested then):
//! `open(O_CREAT)` through a DANGLING symlink does not create the target
//! yet — it returns `ENOENT` (loud, never silently wrong data).

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::accounting::{StatFsData, VfsAccounting};
use crate::content::{FileContent, CHUNK_SIZE, CHUNK_OVERHEAD_BYTES, FLAT_MAX};
use crate::dirent::{DirChildren, DIRENT_NAME_INLINE_MAX};
use crate::inode::{InodeArena, InodeKind};
use crate::observer::{MutationEvent, MutationHeader, VfsObserver};
use crate::path::{SplitPath, SYMLOOP_MAX};
use crate::{Errno, VResult, NAME_MAX};

/// Linux `O_*` flag values exactly as the Node binding passes them
/// (ABI §7.3; `poc/p1-poc/worker.mjs` O table).
pub struct OpenModes;
impl OpenModes {
    pub const RDONLY: u32 = 0;
    pub const WRONLY: u32 = 1;
    pub const RDWR: u32 = 2;
    pub const ACC_MASK: u32 = 3;
    pub const CREAT: u32 = 64;
    pub const EXCL: u32 = 128;
    pub const TRUNC: u32 = 512;
    pub const APPEND: u32 = 1024;
}

/// Per-process umask default (spec §3; differential parity 0644/0755).
pub const DEFAULT_UMASK: u16 = 0o022;

/// UV_DIRENT kinds (ABI §7.3 readdir encoding).
pub const DIRENT_UNKNOWN: u8 = 0;
pub const DIRENT_FILE: u8 = 1;
pub const DIRENT_DIR: u8 = 2;
pub const DIRENT_LINK: u8 = 3;

const MAX_MOUNTS: usize = 4;
const DIR_SIZE_CONST: u64 = 4096; // spec §3 fidelity choice (kept)
const BLKSIZE: u64 = 65_536;

/// The 18-field stat result in ABI §7.3 order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatData {
    pub dev: u64,
    pub mode: u64,
    pub nlink: u64,
    pub uid: u64,
    pub gid: u64,
    pub rdev: u64,
    pub blksize: u64,
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime_sec: i64,
    pub atime_nsec: i32,
    pub mtime_sec: i64,
    pub mtime_nsec: i32,
    pub ctime_sec: i64,
    pub ctime_nsec: i32,
    pub birthtime_sec: i64,
    pub birthtime_nsec: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDirent {
    pub name: Vec<u8>,
    pub kind: u8,
    pub ino: u64,
}

enum Payload {
    Dir(DirChildren),
    File(FileContent),
    Symlink(Box<[u8]>),
}

pub struct MountEntry {
    pub prefix: Vec<u8>,
    pub dev: u8,
    pub root_idx: u32,
}

/// Shared read-only view for the null tier (B D4a: a 0-byte file with
/// `data = 0` — NO payload slot). Materialized into a real payload on the
/// first mutation (`file_content_mut`).
static NULL_TIER: FileContent = FileContent::Empty;

/// One VFS = the single source of truth (spec §1).
pub struct Vfs {
    inodes: InodeArena,
    payloads: Vec<Option<Payload>>,
    payload_free: Vec<u32>,
    /// Global monotonic dentry sequence (readdir cookie domain).
    dseq: u64,
    /// ESM/SW generation counter (spec §1).
    epoch: u64,
    /// Gap-free mutation seq spine (journal idempotency domain).
    mseq: u64,
    mounts: Vec<MountEntry>, // ≤ MAX_MOUNTS
    accounting: VfsAccounting,
    observer: Option<Box<dyn VfsObserver>>,
    root_idx: u32,
}

impl Vfs {
    pub fn new(now_ns: i64) -> Vfs {
        let inodes = InodeArena::with_root(now_ns, 1, 0o755);
        let root_idx = 0;
        let mut vfs = Vfs {
            inodes,
            payloads: Vec::new(),
            payload_free: Vec::new(),
            dseq: 0,
            epoch: 0,
            mseq: 0,
            mounts: Vec::new(),
            accounting: VfsAccounting::default(),
            observer: None,
            root_idx,
        };
        // Root payload = empty DirChildren; dev 1 = the "/" mount.
        let p = vfs.alloc_payload(Payload::Dir(DirChildren::new()));
        vfs.inodes.get_mut(root_idx).data = p;
        vfs.mounts.push(MountEntry {
            prefix: vec![b'/'],
            dev: 1,
            root_idx,
        });
        let _ = vfs.accounting.charge_inode(); // root is live from boot
        vfs
    }

    // ---- accessors -------------------------------------------------------

    pub fn root_idx(&self) -> u32 {
        self.root_idx
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn mseq(&self) -> u64 {
        self.mseq
    }

    pub fn accounting(&self) -> &VfsAccounting {
        &self.accounting
    }

    pub fn statfs(&self) -> StatFsData {
        self.accounting.statfs()
    }

    pub fn set_observer(&mut self, obs: Option<Box<dyn VfsObserver>>) {
        self.observer = obs;
    }

    pub fn external_ino(&self, idx: u32) -> u64 {
        self.inodes.get(idx).ino
    }

    // ---- payload slab ----------------------------------------------------

    /// Allocate a payload slot; returns the 1-BASED handle (inode `data`
    /// convention: 0 = no payload).
    fn alloc_payload(&mut self, p: Payload) -> u32 {
        match self.payload_free.pop() {
            Some(i) => {
                self.payloads[i as usize] = Some(p);
                i + 1
            }
            None => {
                crate::grow_125(&mut self.payloads, 4);
                self.payloads.push(Some(p));
                self.payloads.len() as u32 // 1-based: len == idx+1
            }
        }
    }

    /// Release a 1-based payload handle.
    fn release_payload(&mut self, handle: u32) {
        debug_assert!(handle >= 1);
        self.payloads[handle as usize - 1] = None;
        self.payload_free.push(handle - 1);
    }

    fn dir_children(&self, idx: u32) -> VResult<&DirChildren> {
        match self.payloads.get(self.inodes.get(idx).data as usize - 1) {
            Some(Some(Payload::Dir(d))) => Ok(d),
            _ => Err(Errno::Enotdir),
        }
    }

    fn dir_children_mut(&mut self, idx: u32) -> VResult<&mut DirChildren> {
        let p = self.inodes.get(idx).data;
        if p == 0 {
            return Err(Errno::Enotdir);
        }
        match self.payloads.get_mut(p as usize - 1) {
            Some(Some(Payload::Dir(d))) => Ok(d),
            _ => Err(Errno::Enotdir),
        }
    }

    fn file_content(&self, idx: u32) -> VResult<&FileContent> {
        let d = self.inodes.get(idx).data;
        if d == 0 {
            // Distinguish "null-tier file" from "not a file": the kind
            // check lives in the payload table, so re-check here.
            if self.inodes.get(idx).is_dir() {
                return Err(Errno::Eisdir);
            }
            if !self.inodes.get(idx).is_file() {
                return Err(Errno::Ebadf);
            }
            return Ok(&NULL_TIER);
        }
        match self.payloads.get(d as usize - 1) {
            Some(Some(Payload::File(c))) => Ok(c),
            Some(Some(Payload::Dir(_))) => Err(Errno::Eisdir),
            _ => Err(Errno::Ebadf),
        }
    }

    fn file_content_mut(&mut self, idx: u32) -> VResult<&mut FileContent> {
        let p = self.inodes.get(idx).data;
        if p == 0 {
            // Null tier: materialize the payload on first mutation (the
            // pre-flight in write_at/ftruncate saw the shared EMPTY view,
            // whose charges match a fresh Empty payload).
            if !self.inodes.get(idx).is_file() {
                return Err(Errno::Ebadf);
            }
            let slot = self.alloc_payload(Payload::File(FileContent::Empty));
            self.inodes.get_mut(idx).data = slot;
            return self.file_content_mut(idx);
        }
        match self.payloads.get_mut(p as usize - 1) {
            Some(Some(Payload::File(c))) => Ok(c),
            Some(Some(Payload::Dir(_))) => Err(Errno::Eisdir),
            _ => Err(Errno::Ebadf),
        }
    }

    fn symlink_target(&self, idx: u32) -> VResult<&[u8]> {
        match self.payloads.get(self.inodes.get(idx).data as usize - 1) {
            Some(Some(Payload::Symlink(t))) => Ok(t),
            _ => Err(Errno::Einval),
        }
    }

    // ---- resolution (spec §2) --------------------------------------------

    /// Resolve `path` from `start`. `follow_final` controls the LAST
    /// component only (lstat/rename/unlink/rmdir/link/readlink pass false).
    /// Symlink budget 40 (ELOOP); structural `..`; trailing slash requires
    /// a directory (ENOTDIR).
    pub fn resolve(&self, start: u32, path: &[u8], follow_final: bool) -> VResult<u32> {
        let sp = SplitPath::new(path)?;
        let base = if sp.absolute { self.root_idx } else { start };
        let idx = self.resolve_inner(base, path, follow_final, 0)?;
        if sp.trailing_slash && !self.inodes.get(idx).is_dir() {
            return Err(Errno::Enotdir);
        }
        Ok(idx)
    }

    fn resolve_inner(&self, start: u32, path: &[u8], follow_final: bool, depth: u32) -> VResult<u32> {
        let mut cur = start;
        let mut rest = path;
        loop {
            while rest.first() == Some(&b'/') {
                rest = &rest[1..];
            }
            if rest.is_empty() {
                return Ok(cur);
            }
            if !self.inodes.get(cur).is_dir() {
                return Err(Errno::Enotdir);
            }
            let (comp, tail) = match rest.iter().position(|&b| b == b'/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, &rest[rest.len()..]),
            };
            let is_final = tail.iter().all(|&b| b == b'/');
            match comp {
                b"." => {
                    rest = tail;
                }
                b".." => {
                    if cur != self.root_idx {
                        cur = self.inodes.get(cur).parent_idx;
                    }
                    rest = tail;
                }
                name => {
                    let children = self.dir_children(cur)?;
                    let Some(slot) = children.lookup(name) else {
                        return Err(Errno::Enoent);
                    };
                    let child_idx = children.ino_of(slot);
                    let follow = !is_final || follow_final;
                    if follow && self.inodes.get(child_idx).is_symlink() {
                        if depth >= SYMLOOP_MAX {
                            return Err(Errno::Eloop);
                        }
                        let target = self.symlink_target(child_idx)?.to_vec();
                        let base = if target.first() == Some(&b'/') {
                            self.root_idx
                        } else {
                            cur
                        };
                        let resolved = self.resolve_inner(base, &target, true, depth + 1)?;
                        cur = resolved;
                        rest = tail;
                        continue;
                    }
                    cur = child_idx;
                    rest = tail;
                }
            }
        }
    }

    /// Resolve the PARENT directory of `path`'s final component + the name
    /// (stack-copied — no outstanding borrows). `Err(Eexist)` when the path
    /// has no creatable final component ("/" or `..`-terminated) and the
    /// target exists.
    fn resolve_parent(&self, start: u32, path: &[u8]) -> VResult<(u32, [u8; NAME_MAX], usize)> {
        let sp = SplitPath::new(path)?;
        if sp.is_root() {
            let _ = self.resolve(start, path, true)?; // propagate ENOENT/ENOTDIR
            return Err(Errno::Eexist); // exists (root or ..-terminated dir)
        }
        let (name, off) = sp.split_last().expect("checked above");
        debug_assert!(!name.is_empty() && name.len() <= NAME_MAX);
        let prefix: &[u8] = if off == 0 {
            b""
        } else {
            &path[..off - 1]
        };
        let parent_path: &[u8] = if prefix.is_empty() {
            if sp.absolute {
                b"/"
            } else {
                b""
            }
        } else {
            prefix
        };
        let parent = self.resolve_inner(
            if sp.absolute { self.root_idx } else { start },
            parent_path,
            true,
            0,
        )?;
        if !self.inodes.get(parent).is_dir() {
            return Err(Errno::Enotdir);
        }
        let mut buf = [0u8; NAME_MAX];
        buf[..name.len()].copy_from_slice(name);
        Ok((parent, buf, name.len()))
    }

    // ---- inode lifecycle helpers -----------------------------------------

    fn is_mount_root(&self, idx: u32) -> bool {
        self.mounts.iter().any(|m| m.root_idx == idx)
    }

    /// Free a fully-dead inode (nlink == 0 && open_count == 0): refund its
    /// content/overhead bytes (dirent refunds already happened at dirent
    /// removal — dirs are only rmdir-able when empty) and release the
    /// payload + slot.
    fn release_if_dead(&mut self, idx: u32) {
        let n = self.inodes.get(idx);
        if n.nlink != 0 || n.open_count != 0 {
            return;
        }
        let data = n.data;
        if data != 0 {
            let (content, overhead) = match self.payloads[data as usize - 1].as_ref() {
                Some(Payload::File(c)) => (c.logical_size(), c.overhead_bytes()),
                Some(Payload::Symlink(t)) => (t.len() as u64, 0),
                _ => (0, 0), // Dir payloads die empty — dirents already refunded
            };
            self.accounting.refund(content, overhead, 0);
            self.release_payload(data);
        }
        self.inodes.free(idx);
        self.accounting.refund_inode();
    }

    fn bump_parent_times(&mut self, parent: u32, now: i64) {
        let p = self.inodes.get_mut(parent);
        p.mtime_ns = now;
        p.ctime_ns = now;
    }

    fn commit_event(&mut self, ev: MutationEvent) {
        self.epoch += 1;
        self.mseq += 1;
        if let Some(obs) = self.observer.as_mut() {
            let hdr = MutationHeader {
                seq: self.mseq,
                pid: 0,
                epoch: self.epoch,
            };
            obs.on_commit(&hdr, &ev);
        }
    }

    /// Charge inode + dirent for a fresh entry; on failure nothing changed.
    fn charge_new_entry(&mut self, name_len: usize) -> VResult<()> {
        self.accounting.charge_inode()?;
        let nb = boxed_name_bytes(name_len);
        if let Err(e) = self.accounting.charge_dirent(nb) {
            self.accounting.refund_inode();
            return Err(e);
        }
        Ok(())
    }

    // ---- create-family ops ------------------------------------------------

    /// mkdir (non-recursive). EEXIST on ANY existing path (item 21);
    /// ENOENT/ENOTDIR per POSIX.
    pub fn mkdir(&mut self, start: u32, path: &[u8], mode: u16, umask: u16, now: i64) -> VResult<u32> {
        let (parent, name_buf, name_len) = self.resolve_parent(start, path)?;
        let name = &name_buf[..name_len];
        if self.dir_children(parent)?.contains(name) {
            return Err(Errno::Eexist);
        }
        self.charge_new_entry(name_len)?;
        // Mutate.
        let dev = self.inodes.get(parent).dev;
        let idx = self.inodes.alloc(InodeKind::Dir, mode & !umask, dev, now);
        self.inodes.get_mut(idx).parent_idx = parent;
        self.inodes.get_mut(idx).nlink = 2; // '.' + parent entry (Linux parity)
        let p = self.alloc_payload(Payload::Dir(DirChildren::new()));
        self.inodes.get_mut(idx).data = p;
        self.dseq += 1;
        let seq = self.dseq;
        self.dir_children_mut(parent)?
            .insert(name, idx, seq)
            .expect("pre-checked");
        self.inodes.get_mut(parent).nlink += 1; // dirs: 2 + subdirs
        self.bump_parent_times(parent, now);
        self.commit_event(MutationEvent::Create {
            ino: self.inodes.get(idx).ino,
            parent_ino: self.inodes.get(parent).ino,
            name,
        });
        Ok(idx)
    }

    /// symlink(target, path). Target stored raw (never validated/resolved).
    pub fn symlink(&mut self, start: u32, path: &[u8], target: &[u8], now: i64) -> VResult<u32> {
        if target.len() > crate::PATH_MAX {
            return Err(Errno::Enametoolong);
        }
        if target.contains(&0) {
            return Err(Errno::Einval);
        }
        let (parent, name_buf, name_len) = self.resolve_parent(start, path)?;
        let name = &name_buf[..name_len];
        if self.dir_children(parent)?.contains(name) {
            return Err(Errno::Eexist);
        }
        // Pre-flight: entry + target bytes (content class — stat size).
        self.charge_new_entry(name_len)?;
        if let Err(e) = self.accounting.charge(target.len() as u64, 0, 0) {
            let nb = boxed_name_bytes(name_len);
            self.accounting.refund_dirent(nb);
            self.accounting.refund_inode();
            return Err(e);
        }
        // Mutate.
        let dev = self.inodes.get(parent).dev;
        let idx = self.inodes.alloc(InodeKind::Symlink, 0o777, dev, now);
        self.inodes.get_mut(idx).parent_idx = parent;
        self.inodes.get_mut(idx).nlink = 1; // one dirent
        self.inodes.get_mut(idx).size = target.len() as u64;
        let p = self.alloc_payload(Payload::Symlink(Box::from(target)));
        self.inodes.get_mut(idx).data = p;
        self.dseq += 1;
        let seq = self.dseq;
        self.dir_children_mut(parent)?
            .insert(name, idx, seq)
            .expect("pre-checked");
        self.bump_parent_times(parent, now);
        self.commit_event(MutationEvent::SymlinkCreate {
            ino: self.inodes.get(idx).ino,
            parent_ino: self.inodes.get(parent).ino,
            name,
        });
        Ok(idx)
    }

    /// open. Returns the pinned inode idx (the OFD/fd layer wraps this;
    /// `close`/`unpin` releases). Flags matrix per spec §6 / items 1–3.
    /// Follows symlinks (final included). O_CREAT on a DANGLING symlink
    /// returns ENOENT (documented interim — P2-005-008).
    pub fn open(
        &mut self,
        start: u32,
        path: &[u8],
        flags: u32,
        mode: u16,
        umask: u16,
        now: i64,
    ) -> VResult<u32> {
        let acc = flags & OpenModes::ACC_MASK;
        if acc == OpenModes::ACC_MASK {
            return Err(Errno::Einval); // Linux: RDONLY|WRONLY is invalid
        }
        let create = flags & OpenModes::CREAT != 0;
        let excl = flags & OpenModes::EXCL != 0;
        let trunc = flags & OpenModes::TRUNC != 0;
        // FD_SPEC §6 rows 4-6 (differentials E-7/E-8/D4, Linux 5.10.134 —
        // poc/p2-005/results/fd-differentials-linux.txt):
        // - O_TRUNC applies regardless of access mode (O_RDONLY|O_TRUNC
        //   succeeds and truncates); the old EINVAL row was a divergence.
        // - O_EXCL without O_CREAT is a no-op bit (checked at the no-create
        //   path below); the old EEXIST row was a divergence.
        // - dir + O_TRUNC ⇒ EISDIR (was EINVAL) — handled with the dir
        //   write-access gate in both paths.

        if create {
            match self.resolve(start, path, true) {
                Ok(idx) => {
                    // Exists (following symlinks).
                    if excl {
                        return Err(Errno::Eexist);
                    }
                    if self.inodes.get(idx).is_dir() {
                        // Write access OR O_TRUNC on a directory ⇒ EISDIR
                        // (differential D2-D5: any write-open of a dir, and
                        // dir + O_RDONLY|O_TRUNC, both EISDIR on Linux).
                        if acc != OpenModes::RDONLY || trunc {
                            return Err(Errno::Eisdir);
                        }
                    } else if trunc {
                        // Row 5 (differential E-8): truncation is NOT gated on
                        // write access — O_RDONLY|O_TRUNC truncates.
                        self.ftruncate_idx(idx, 0, now)?;
                    }
                    self.inodes.get_mut(idx).open_count += 1;
                    return Ok(idx);
                }
                Err(Errno::Enoent) => {
                    // Missing (or a dangling symlink's target): create when
                    // the final NAME is genuinely free.
                    let (parent, name_buf, name_len) = self.resolve_parent(start, path)?;
                    let name = &name_buf[..name_len];
                    if self.dir_children(parent)?.contains(name) {
                        // Dangling symlink at the final name.
                        return Err(Errno::Enoent);
                    }
                    self.charge_new_entry(name_len)?;
                    // Mutate. Null tier (B D4a): a created file starts at
                    // `data = 0` — ZERO payload allocations for the
                    // create-storm workload; the first write materializes
                    // the payload (file_content_mut).
                    let dev = self.inodes.get(parent).dev;
                    let idx = self.inodes.alloc(InodeKind::File, mode & !umask, dev, now);
                    self.inodes.get_mut(idx).nlink = 1; // one dirent
                    self.dseq += 1;
                    let seq = self.dseq;
                    self.dir_children_mut(parent)?
                        .insert(name, idx, seq)
                        .expect("pre-checked");
                    self.bump_parent_times(parent, now);
                    self.inodes.get_mut(idx).open_count += 1;
                    self.commit_event(MutationEvent::Create {
                        ino: self.inodes.get(idx).ino,
                        parent_ino: self.inodes.get(parent).ino,
                        name,
                    });
                    return Ok(idx);
                }
                Err(e) => return Err(e),
            }
        }

        // Plain open (no O_CREAT): full follow resolution. O_EXCL without
        // O_CREAT is a NO-OP on Linux (differential E-7: existing file opens
        // fine, missing ⇒ ENOENT) — the old EEXIST row was a divergence and
        // is deliberately removed (FD_SPEC §6 row 4).
        let idx = self.resolve(start, path, true)?;
        if self.inodes.get(idx).is_dir() {
            // D2-D5: write access or O_TRUNC on a dir ⇒ EISDIR.
            if acc != OpenModes::RDONLY || trunc {
                return Err(Errno::Eisdir);
            }
        } else if trunc {
            // Row 5 (differential E-8): truncates regardless of access mode.
            self.ftruncate_idx(idx, 0, now)?;
        }
        self.inodes.get_mut(idx).open_count += 1;
        Ok(idx)
    }

    /// close / unpin (cwd pins use the same). Last-close reclaims unlinked
    /// files (POSIX, item 29).
    pub fn close(&mut self, idx: u32) -> VResult<()> {
        if self.inodes.get(idx).open_count == 0 {
            return Err(Errno::Ebadf);
        }
        self.inodes.get_mut(idx).open_count -= 1;
        self.release_if_dead(idx);
        Ok(())
    }

    /// Pin an inode as a cwd-style reference (AV-3: cwd holds a reference).
    pub fn pin(&mut self, idx: u32) {
        self.inodes.get_mut(idx).open_count += 1;
    }

    // ---- data ops ----------------------------------------------------------

    /// Read up to `buf.len()` at `pos`; returns bytes read (0 at EOF —
    /// item 7). Updates atime (spec §3). EISDIR on directories (item 8).
    pub fn read_at(&mut self, idx: u32, pos: u64, buf: &mut [u8], now: i64) -> VResult<usize> {
        let n = {
            let c = self.file_content(idx)?;
            c.read_at(pos, buf)
        };
        self.inodes.get_mut(idx).atime_ns = now;
        Ok(n)
    }

    /// Write at `pos` (pwrite-style; the fd offset is the OFD layer's
    /// concern). Pre-flights EFBIG + ENOSPC; a zero-length write is a
    /// verified no-op (POSIX: does not extend).
    pub fn write_at(&mut self, idx: u32, pos: u64, data: &[u8], now: i64) -> VResult<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let new_end = pos.saturating_add(data.len() as u64);
        if new_end == u64::MAX {
            return Err(Errno::Efbig); // hostile position: saturates past every ceiling
        }
        let charged = self.file_content(idx)?.growth_charge(pos, data.len());
        self.accounting.check_per_file(new_end)?;
        if charged.content > 0 || charged.overhead > 0 {
            self.accounting
                .charge(charged.content as u64, charged.overhead as u64, 0)?;
        }
        let delta = self.file_content_mut(idx)?.write_at(pos, data, charged);
        debug_assert_eq!(delta, charged, "pre-flight charge must equal actual");
        let node = self.inodes.get_mut(idx);
        // Overwrite-in-place must not shorten the file: a mid-file short
        // write (e.g. 2 B at offset 4 of an 8-B file) has new_end < size —
        // the size only ever GROWS here. (Found live by the P2-005-005 FD
        // suite's independent-opens test — T-FD-class falsifier.)
        node.size = node.size.max(new_end);
        node.mtime_ns = now;
        node.ctime_ns = now;
        self.commit_event(MutationEvent::Write {
            ino: self.inodes.get(idx).ino,
            pos,
            len: data.len() as u64,
        });
        Ok(data.len())
    }

    /// O_APPEND write — atomically positioned at EOF (single-threaded
    /// kernel ⇒ op-atomic; item 11: append wins over any position).
    pub fn append(&mut self, idx: u32, data: &[u8], now: i64) -> VResult<usize> {
        let at = self.file_content(idx)?.logical_size();
        self.write_at(idx, at, data, now)
    }

    /// ftruncate by inode. Shrink frees (refund); grow zero-fills (item 35);
    /// offsets of open OFDs unmoved (POSIX).
    pub fn ftruncate_idx(&mut self, idx: u32, len: u64, now: i64) -> VResult<()> {
        let old_size = self.file_content(idx)?.logical_size(); // EISDIR on dirs
        self.accounting.check_per_file(len)?;
        if len > old_size {
            // Grow: pre-flight the increase + chunk-overhead delta.
            let old_ovh = self.file_content(idx)?.overhead_bytes();
            let new_ovh = if len > FLAT_MAX {
                (((len as usize) + CHUNK_SIZE - 1) / CHUNK_SIZE) as u64 * CHUNK_OVERHEAD_BYTES
            } else {
                0
            };
            self.accounting
                .charge(len - old_size, new_ovh.saturating_sub(old_ovh), 0)?;
        }
        let delta = self.file_content_mut(idx)?.truncate(len);
        if delta.content < 0 || delta.overhead < 0 {
            self.accounting
                .refund((-delta.content) as u64, (-delta.overhead).max(0) as u64, 0);
        }
        self.inodes.get_mut(idx).size = len;
        self.inodes.get_mut(idx).mtime_ns = now;
        self.inodes.get_mut(idx).ctime_ns = now;
        self.commit_event(MutationEvent::Truncate {
            ino: self.inodes.get(idx).ino,
            old_size,
            new_size: len,
        });
        Ok(())
    }

    /// truncate by path: NO create — missing ⇒ ENOENT (item 35), dir ⇒
    /// EISDIR.
    pub fn truncate_path(&mut self, start: u32, path: &[u8], len: u64, now: i64) -> VResult<()> {
        let idx = self.resolve(start, path, true)?;
        if self.inodes.get(idx).is_dir() {
            return Err(Errno::Eisdir);
        }
        self.ftruncate_idx(idx, len, now)
    }

    // ---- name ops -----------------------------------------------------------

    /// unlink. Dir ⇒ EISDIR (item 27). Open fds keep the inode alive
    /// (item 29). "/"-class paths ⇒ EISDIR.
    pub fn unlink(&mut self, start: u32, path: &[u8], now: i64) -> VResult<()> {
        let (parent, name_buf, name_len) = match self.resolve_parent(start, path) {
            Ok(v) => v,
            Err(Errno::Eexist) => return Err(Errno::Eisdir), // root/..-final: a directory
            Err(e) => return Err(e),
        };
        let name = &name_buf[..name_len];
        let Some(target) = self.dir_children(parent)?.lookup(name) else {
            return Err(Errno::Enoent);
        };
        let idx = self.dir_children(parent)?.ino_of(target);
        if self.inodes.get(idx).is_dir() {
            return Err(Errno::Eisdir);
        }
        // Mutations.
        let removed = self
            .dir_children_mut(parent)?
            .remove(name)
            .expect("looked up");
        self.accounting.refund_dirent(removed.name_bytes as u64);
        self.inodes.get_mut(idx).nlink = self.inodes.get(idx).nlink.saturating_sub(1);
        self.bump_parent_times(parent, now);
        let ino = self.inodes.get(idx).ino;
        let parent_ino = self.inodes.get(parent).ino;
        self.release_if_dead(idx);
        self.commit_event(MutationEvent::Unlink {
            ino,
            parent_ino,
            name,
            replaced: false,
        });
        Ok(())
    }

    /// rmdir. ENOTEMPTY on children; ENOTDIR on files; EBUSY on the root /
    /// mount roots; survives as an orphan while cwd-pinned (POSIX).
    pub fn rmdir(&mut self, start: u32, path: &[u8], now: i64) -> VResult<()> {
        let (parent, name_buf, name_len) = match self.resolve_parent(start, path) {
            Ok(v) => v,
            Err(Errno::Eexist) => return Err(Errno::Ebusy), // the tree root
            Err(e) => return Err(e),
        };
        let name = &name_buf[..name_len];
        let Some(target) = self.dir_children(parent)?.lookup(name) else {
            return Err(Errno::Enoent);
        };
        let idx = self.dir_children(parent)?.ino_of(target);
        if !self.inodes.get(idx).is_dir() {
            return Err(Errno::Enotdir);
        }
        if self.is_mount_root(idx) {
            return Err(Errno::Ebusy);
        }
        if !self.dir_children(idx)?.is_empty() {
            return Err(Errno::Enotempty);
        }
        // Mutations.
        let removed = self
            .dir_children_mut(parent)?
            .remove(name)
            .expect("looked up");
        self.accounting.refund_dirent(removed.name_bytes as u64);
        self.inodes.get_mut(idx).nlink = 0; // 2 − self − parent entry
        self.inodes.get_mut(parent).nlink = self.inodes.get(parent).nlink.saturating_sub(1);
        self.bump_parent_times(parent, now);
        let ino = self.inodes.get(idx).ino;
        let parent_ino = self.inodes.get(parent).ino;
        self.release_if_dead(idx); // survives if cwd-pinned (open_count > 0)
        self.commit_event(MutationEvent::Unlink {
            ino,
            parent_ino,
            name,
            replaced: false,
        });
        Ok(())
    }

    /// rename — the full POSIX matrix (item 28) + the names_bytes
    /// pre-check (AV-19) + mount-root EBUSY + ancestry EINVAL. Identity
    /// (ino) survives; O(1) subtree move.
    pub fn rename(&mut self, start: u32, old_path: &[u8], new_path: &[u8], now: i64) -> VResult<()> {
        let (old_parent, old_buf, old_len) = match self.resolve_parent(start, old_path) {
            Ok(v) => v,
            Err(Errno::Eexist) => return Err(Errno::Ebusy), // the root is not renamable
            Err(e) => return Err(e),
        };
        let (new_parent, new_buf, new_len) = match self.resolve_parent(start, new_path) {
            Ok(v) => v,
            Err(Errno::Eexist) => return Err(Errno::Eexist), // target exists ("/"-class)
            Err(e) => return Err(e),
        };
        let old_name = &old_buf[..old_len];
        let new_name = &new_buf[..new_len];

        let Some(old_slot) = self.dir_children(old_parent)?.lookup(old_name) else {
            return Err(Errno::Enoent);
        };
        let old_idx = self.dir_children(old_parent)?.ino_of(old_slot);
        if self.is_mount_root(old_idx) {
            return Err(Errno::Ebusy); // mount roots are not renamable
        }
        let old_is_dir = self.inodes.get(old_idx).is_dir();

        let new_slot = self.dir_children(new_parent)?.lookup(new_name);
        let new_idx = new_slot.map(|s| self.dir_children(new_parent).unwrap().ino_of(s));

        if let Some(ni) = new_idx {
            if old_parent == new_parent && old_idx == ni {
                return Ok(()); // same entry: POSIX no-op success
            }
            if self.is_mount_root(ni) {
                return Err(Errno::Ebusy);
            }
            let new_is_dir = self.inodes.get(ni).is_dir();
            if old_is_dir && !new_is_dir {
                return Err(Errno::Enotdir);
            }
            if !old_is_dir && new_is_dir {
                return Err(Errno::Eisdir);
            }
            if new_is_dir && !self.dir_children(ni)?.is_empty() {
                return Err(Errno::Enotempty);
            }
        }
        // Ancestry: new inside old ⇒ EINVAL.
        if old_is_dir {
            let mut cur = new_parent;
            let mut hops = 0usize;
            loop {
                if cur == old_idx {
                    return Err(Errno::Einval);
                }
                if cur == self.root_idx || hops > 4096 {
                    break;
                }
                let next = self.inodes.get(cur).parent_idx;
                if next == cur {
                    break;
                }
                cur = next;
                hops += 1;
            }
        }

        // AV-19 names_bytes pre-check: charge the NEW dirent (residency
        // check included) BEFORE touching the old entry — a mid-op ENOSPC
        // must never lose the source.
        let new_nb = boxed_name_bytes(new_len);
        self.accounting.charge_dirent(new_nb)?;

        // Mutations.
        let old_ino = self.inodes.get(old_idx).ino;
        let old_parent_ino = self.inodes.get(old_parent).ino;
        let new_parent_ino = self.inodes.get(new_parent).ino;
        let replaced_ino = new_idx.map(|ni| self.inodes.get(ni).ino);

        if let Some(ni) = new_idx {
            let removed = self
                .dir_children_mut(new_parent)?
                .remove(new_name)
                .expect("checked");
            self.accounting.refund_dirent(removed.name_bytes as u64);
            self.inodes.get_mut(ni).nlink = self.inodes.get(ni).nlink.saturating_sub(1);
        }
        self.dseq += 1;
        let seq = self.dseq;
        self.dir_children_mut(new_parent)?
            .insert(new_name, old_idx, seq)
            .expect("checked");
        let removed_old = self
            .dir_children_mut(old_parent)?
            .remove(old_name)
            .expect("looked up");
        self.accounting.refund_dirent(removed_old.name_bytes as u64);

        if old_is_dir {
            self.inodes.get_mut(old_idx).parent_idx = new_parent;
            if old_parent != new_parent {
                self.inodes.get_mut(old_parent).nlink =
                    self.inodes.get(old_parent).nlink.saturating_sub(1);
                self.inodes.get_mut(new_parent).nlink += 1;
            }
        }
        self.inodes.get_mut(old_idx).ctime_ns = now;
        self.bump_parent_times(old_parent, now);
        if old_parent != new_parent {
            self.bump_parent_times(new_parent, now);
        }
        if let Some(ni) = new_idx {
            self.release_if_dead(ni);
        }
        self.commit_event(MutationEvent::Rename {
            ino: old_ino,
            old_parent_ino,
            old_name,
            new_parent_ino,
            new_name,
            replaced_ino,
        });
        Ok(())
    }

    /// link(2) — TRUE hardlink (ruling 0-4: never EXDEV; EPERM on dirs;
    /// EEXIST on existing target; shared inode/identity, item 20).
    pub fn link(&mut self, start: u32, old_path: &[u8], new_path: &[u8], now: i64) -> VResult<()> {
        let old_idx = match self.resolve(start, old_path, false) {
            Ok(v) => v,
            Err(Errno::Eexist) => return Err(Errno::Eperm), // the root is a dir
            Err(e) => return Err(e),
        };
        if self.inodes.get(old_idx).is_dir() {
            return Err(Errno::Eperm); // Linux parity
        }
        let (new_parent, name_buf, name_len) = match self.resolve_parent(start, new_path) {
            Ok(v) => v,
            Err(Errno::Eexist) => return Err(Errno::Eexist),
            Err(e) => return Err(e),
        };
        let name = &name_buf[..name_len];
        if self.dir_children(new_parent)?.contains(name) {
            return Err(Errno::Eexist);
        }
        let nb = boxed_name_bytes(name_len);
        self.accounting.charge_dirent(nb)?;
        // Mutate.
        self.dseq += 1;
        let seq = self.dseq;
        self.dir_children_mut(new_parent)?
            .insert(name, old_idx, seq)
            .expect("pre-checked");
        self.inodes.get_mut(old_idx).nlink += 1;
        self.inodes.get_mut(old_idx).ctime_ns = now;
        self.bump_parent_times(new_parent, now);
        self.commit_event(MutationEvent::Link {
            ino: self.inodes.get(old_idx).ino,
            parent_ino: self.inodes.get(new_parent).ino,
            name,
        });
        Ok(())
    }

    /// readlink — raw target bytes via callback (zero-copy; the bridge
    /// writes them straight to the result area). EINVAL on non-symlink
    /// (item 34).
    pub fn readlink_with<F: FnOnce(&[u8])>(&self, start: u32, path: &[u8], f: F) -> VResult<()> {
        let idx = match self.resolve(start, path, false) {
            Ok(v) => v,
            Err(Errno::Eexist) => return Err(Errno::Einval), // "/" is not a symlink
            Err(e) => return Err(e),
        };
        let target = self.symlink_target(idx).map_err(|_| Errno::Einval)?;
        f(target);
        Ok(())
    }

    // ---- metadata ops --------------------------------------------------------

    pub fn stat(&self, start: u32, path: &[u8]) -> VResult<StatData> {
        let idx = self.resolve(start, path, true)?;
        self.stat_idx(idx)
    }

    pub fn lstat(&self, start: u32, path: &[u8]) -> VResult<StatData> {
        let idx = self.resolve(start, path, false)?;
        self.stat_idx(idx)
    }

    /// Is this pinned idx a directory? (FD-layer kind resolution —
    /// P2_005_FD_SPEC §7.)
    pub fn idx_is_dir(&self, idx: u32) -> bool {
        self.inodes.get(idx).is_dir()
    }

    /// Logical content size of a pinned idx (append positioning — FD_SPEC §5
    /// row 5). EISDIR on directories (append only targets files).
    pub fn idx_logical_size(&self, idx: u32) -> VResult<u64> {
        let n = self.inodes.get(idx);
        if n.is_free() {
            return Err(Errno::Enoent);
        }
        if n.is_dir() {
            return Err(Errno::Eisdir);
        }
        Ok(n.size)
    }

    pub fn stat_idx(&self, idx: u32) -> VResult<StatData> {
        let n = self.inodes.get(idx);
        if n.is_free() {
            return Err(Errno::Enoent);
        }
        let (at_s, at_ns) = split_ns(n.atime_ns);
        let (mt_s, mt_ns) = split_ns(n.mtime_ns);
        let (ct_s, ct_ns) = split_ns(n.ctime_ns);
        let (bt_s, bt_ns) = split_ns(n.birthtime_ns);
        let size = if n.is_dir() { DIR_SIZE_CONST } else { n.size };
        Ok(StatData {
            dev: n.dev as u64,
            mode: (n.ifmt_bits() | n.perm as u32) as u64,
            nlink: n.nlink as u64,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: BLKSIZE,
            ino: n.ino,
            size,
            blocks: (size + BLKSIZE - 1) / BLKSIZE,
            atime_sec: at_s,
            atime_nsec: at_ns,
            mtime_sec: mt_s,
            mtime_nsec: mt_ns,
            ctime_sec: ct_s,
            ctime_nsec: ct_ns,
            birthtime_sec: bt_s,
            birthtime_nsec: bt_ns,
        })
    }

    /// chmod (follows symlinks — item 36).
    pub fn chmod(&mut self, start: u32, path: &[u8], perm: u16, now: i64) -> VResult<()> {
        let idx = match self.resolve(start, path, true) {
            Ok(v) => v,
            Err(Errno::Eexist) => {
                return self.chmod_idx(self.root_idx, perm, now);
            }
            Err(e) => return Err(e),
        };
        self.chmod_idx(idx, perm, now)
    }

    pub fn chmod_idx(&mut self, idx: u32, perm: u16, now: i64) -> VResult<()> {
        let p = perm & 0o7777;
        self.inodes.get_mut(idx).perm = p;
        self.inodes.get_mut(idx).ctime_ns = now;
        self.commit_event(MutationEvent::SetMode {
            ino: self.inodes.get(idx).ino,
            perm: p,
        });
        Ok(())
    }

    /// utimes (`follow=true`) / lutimes (`follow=false`). Values are
    /// ns-since-epoch; out-of-i64-ns-range (sec, nsec) inputs are rejected
    /// at the ABI layer with EINVAL (spec §3 range corner — the conversion
    /// uses checked math there).
    pub fn utimes(
        &mut self,
        start: u32,
        path: &[u8],
        follow: bool,
        atime_ns: i64,
        mtime_ns: i64,
        now: i64,
    ) -> VResult<()> {
        let idx = self.resolve(start, path, follow)?;
        {
            let n = self.inodes.get_mut(idx);
            n.atime_ns = atime_ns;
            n.mtime_ns = mtime_ns;
            n.ctime_ns = now;
        }
        self.commit_event(MutationEvent::SetTimes {
            ino: self.inodes.get(idx).ino,
        });
        Ok(())
    }

    // ---- enumeration ---------------------------------------------------------

    /// readdir batch via callback (bridge streams into the RX area).
    /// Returns `(next_cookie, emitted)`; `emitted == 0` ⇒ EOF (ABI §7.3).
    /// Callback: `(name, UV_DIRENT kind, external ino, seq)`. `.`/`..`
    /// are never returned (item 23 — the kernel owns the filter).
    pub fn readdir_with<F: FnMut(&[u8], u8, u64, u64)>(
        &self,
        start: u32,
        path: &[u8],
        cookie: u64,
        max_entries: usize,
        max_bytes: usize,
        mut f: F,
    ) -> VResult<(u64, usize)> {
        let idx = self.resolve(start, path, true)?;
        if !self.inodes.get(idx).is_dir() {
            return Err(Errno::Enotdir);
        }
        let children = self.dir_children(idx)?;
        let inodes = &self.inodes;
        let (next, emitted) =
            children.enumerate(cookie, max_entries, max_bytes, |name, child_idx, seq| {
                let node = inodes.get(child_idx);
                let kind = if node.is_dir() {
                    DIRENT_DIR
                } else if node.is_symlink() {
                    DIRENT_LINK
                } else {
                    DIRENT_FILE
                };
                f(name, kind, node.ino, seq);
            });
        Ok((next, emitted))
    }

    /// Convenience batch API (allocates — tests and non-hot callers).
    pub fn readdir(
        &self,
        start: u32,
        path: &[u8],
        cookie: u64,
        max_entries: usize,
        max_bytes: usize,
    ) -> VResult<(Vec<RawDirent>, u64, bool)> {
        let mut out = Vec::new();
        let (next, emitted) =
            self.readdir_with(start, path, cookie, max_entries, max_bytes, |name, kind, ino, _seq| {
                out.push(RawDirent {
                    name: Vec::from(name),
                    kind,
                    ino,
                });
            })?;
        Ok((out, next, emitted == 0))
    }

    // ---- mounts ----------------------------------------------------------------

    /// Ensure a mount prefix exists as a directory and label it `dev`
    /// (mounts are `st_dev` views over one tree — spec §1/§3.4; NEVER a
    /// device boundary, ruling 0-4). Creates missing dirs (boot layout).
    pub fn ensure_mount(&mut self, prefix: &[u8], dev: u8, now: i64) -> VResult<u32> {
        if self.mounts.len() >= MAX_MOUNTS {
            return Err(Errno::Enospc);
        }
        let sp = SplitPath::new(prefix)?;
        if !sp.absolute {
            return Err(Errno::Einval);
        }
        let mut cur = self.root_idx;
        for comp in sp.components() {
            if comp == b".." {
                cur = self.inodes.get(cur).parent_idx;
                continue;
            }
            let existing = self.dir_children(cur)?.lookup(comp);
            match existing {
                Some(slot) => cur = self.dir_children(cur)?.ino_of(slot),
                None => {
                    // mkdir -p (boot path)
                    self.charge_new_entry(comp.len())?;
                    let dev_here = self.inodes.get(cur).dev;
                    let idx = self.inodes.alloc(InodeKind::Dir, 0o755, dev_here, now);
                    self.inodes.get_mut(idx).parent_idx = cur;
                    self.inodes.get_mut(idx).nlink = 2;
                    let p = self.alloc_payload(Payload::Dir(DirChildren::new()));
                    self.inodes.get_mut(idx).data = p;
                    self.dseq += 1;
                    let seq = self.dseq;
                    self.dir_children_mut(cur)?.insert(comp, idx, seq).expect("unique");
                    self.inodes.get_mut(cur).nlink += 1;
                    cur = idx;
                }
            }
        }
        if !self.inodes.get(cur).is_dir() {
            return Err(Errno::Enotdir);
        }
        if let Some(m) = self.mounts.iter_mut().find(|m| m.root_idx == cur) {
            m.dev = dev;
        } else {
            self.mounts.push(MountEntry {
                prefix: Vec::from(prefix),
                dev,
                root_idx: cur,
            });
        }
        self.inodes.get_mut(cur).dev = dev;
        Ok(cur)
    }

    pub fn mounts(&self) -> &Vec<MountEntry> {
        &self.mounts
    }
}

#[inline]
fn boxed_name_bytes(len: usize) -> u64 {
    if len > DIRENT_NAME_INLINE_MAX {
        len as u64
    } else {
        0
    }
}

#[inline]
fn split_ns(ns: i64) -> (i64, i32) {
    let sec = ns.div_euclid(1_000_000_000);
    let nsec = ns.rem_euclid(1_000_000_000) as i32;
    (sec, nsec)
}
