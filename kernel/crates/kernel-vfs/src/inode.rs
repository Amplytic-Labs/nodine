//! Inode table and identity (spec §1).
//!
//! Frozen layout: 72 B, align 8 — `P2_005_VFS_SPEC.md` §1, layout asserted at
//! compile time below (drift = design regression, B §3.5).
//!
//! Identity (ruling 0-7): the external `ino` is a **global monotonic u64,
//! never reused** — f64-exact below 2⁵³, no ABA window, `realpath`'s
//! `dev:ino` keys are always correct. The internal arena index (`u32`) never
//! leaves the kernel; `cwd` and the journal hold **ino** references, never
//! bare indices.

use alloc::vec::Vec;
use core::mem::size_of;

/// Compiled size of the frozen inode layout.
pub const INODE_SIZE: usize = 72;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InodeKind {
    File = 1,
    Dir = 2,
    Symlink = 3,
}

impl InodeKind {
    pub const fn from_u8(v: u8) -> Option<InodeKind> {
        match v {
            1 => Some(InodeKind::File),
            2 => Some(InodeKind::Dir),
            3 => Some(InodeKind::Symlink),
            _ => None,
        }
    }
}

/// The 72-byte inode (spec §1). `data` is the payload-slab slot + 1 (0 = no
/// payload: 0-byte file, or — invalid for dirs/symlinks after init); kind
/// payloads (children / content / symlink target) live in the side slab so
/// the hot inode stays cache-dense and the layout stays frozen.
#[derive(Debug, Clone)]
#[repr(C, align(8))]
pub struct Inode {
    /// External identity — global monotonic, NEVER reused (ruling 0-7).
    pub ino: u64,
    /// ns since epoch. Exact range 1677–2262; `utimes` outside ⇒ EINVAL
    /// (spec §3, ext4-analogous bound).
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub atime_ns: i64,
    pub birthtime_ns: i64,
    /// Logical size (symlink: target byte length).
    pub size: u64,
    /// Dirent references (files) / 2 + immediate subdirs (dirs, Linux parity).
    pub nlink: u32,
    /// OFD pins — free only when `nlink == 0 && open_count == 0` (POSIX
    /// last-close, spec §1).
    pub open_count: u32,
    /// Payload-slab slot + 1; 0 = none (spec §1).
    pub data: u32,
    /// Dirs only: parent arena idx ('..' + ancestry checks). Files: 0 —
    /// parent is a *directory* concept; hardlinked files have no single
    /// parent (spec §1).
    pub parent_idx: u32,
    /// 0o7777 needs 12 bits (spec §3).
    pub perm: u16,
    pub kind: u8, // InodeKind; 0 = FREE slot (in-band free state, B D1a)
    /// Mount id (≤4 mounts Phase 2) — an `st_dev` label, NOT a device
    /// boundary (erratum AV-1: never EXDEV).
    pub dev: u8,
    /// Persistence-seam dirty flag etc.
    pub flags: u8,
    pub _pad: [u8; 3],
}

// Frozen-layout compile-time asserts (spec §1; B §3.5 falsifier).
const _: () = assert!(size_of::<Inode>() == 72);
const _: () = assert!(core::mem::align_of::<Inode>() == 8);

impl Inode {
    #[inline]
    pub fn kind(&self) -> Option<InodeKind> {
        InodeKind::from_u8(self.kind)
    }

    #[inline]
    pub fn is_free(&self) -> bool {
        self.kind == 0
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.kind == InodeKind::Dir as u8
    }

    #[inline]
    pub fn is_file(&self) -> bool {
        self.kind == InodeKind::File as u8
    }

    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.kind == InodeKind::Symlink as u8
    }

    /// S_IFMT mode bits for `stat` (spec §3).
    #[inline]
    pub fn ifmt_bits(&self) -> u32 {
        match self.kind() {
            Some(InodeKind::File) => 0o100000,
            Some(InodeKind::Dir) => 0o040000,
            Some(InodeKind::Symlink) => 0o120000,
            None => 0,
        }
    }
}

/// Slot arena with in-band FREE state and a LIFO u32 free list (B D1a).
/// Growth 1.25× bounds doubling transients.
pub struct InodeArena {
    slots: Vec<Inode>,
    free: Vec<u32>,
    next_ino: u64,
    live: u32,
}

impl InodeArena {
    /// New arena; slot 0 is reserved as the root directory (created with
    /// `root_dev`, nlink = 2 — the Linux `.` + parent-entry convention,
    /// spec §3).
    pub fn with_root(now_ns: i64, root_dev: u8, root_perm: u16) -> InodeArena {
        let mut arena = InodeArena {
            slots: Vec::new(),
            free: Vec::new(),
            next_ino: 1,
            live: 0,
        };
        let idx = arena.alloc_raw(InodeKind::Dir, root_perm, root_dev, now_ns);
        debug_assert_eq!(idx, 0);
        arena.slots[0].nlink = 2; // '.' + '..' of the tree root
        arena
    }

    fn alloc_raw(
        &mut self,
        kind: InodeKind,
        perm: u16,
        dev: u8,
        now_ns: i64,
    ) -> u32 {
        let ino = self.next_ino;
        self.next_ino += 1;
        let node = Inode {
            ino,
            mtime_ns: now_ns,
            ctime_ns: now_ns,
            atime_ns: now_ns,
            birthtime_ns: now_ns,
            size: 0,
            nlink: 0,
            open_count: 0,
            data: 0,
            parent_idx: 0,
            perm,
            kind: kind as u8,
            dev,
            flags: 0,
            _pad: [0; 3],
        };
        self.live += 1;
        match self.free.pop() {
            Some(idx) => {
                self.slots[idx as usize] = node;
                idx
            }
            None => {
                // D1a: 1.25x growth (NOT Vec's 2x doubling) — the B1
                // heap-live gate falsifies doubling slack at 100k files.
                crate::grow_125(&mut self.slots, 4);
                self.slots.push(node);
                (self.slots.len() - 1) as u32
            }
        }
    }

    /// Allocate a fresh inode. The caller (Vfs) enforces the inode cap
    /// (pre-flight, spec §8) before calling.
    pub fn alloc(&mut self, kind: InodeKind, perm: u16, dev: u8, now_ns: i64) -> u32 {
        self.alloc_raw(kind, perm, dev, now_ns)
    }

    /// Mark a slot free. The caller MUST have verified `nlink == 0 &&
    /// open_count == 0` and released the payload slot already.
    pub fn free(&mut self, idx: u32) {
        debug_assert!((idx as usize) < self.slots.len());
        debug_assert!(!self.slots[idx as usize].is_free());
        debug_assert_eq!(self.slots[idx as usize].nlink, 0);
        debug_assert_eq!(self.slots[idx as usize].open_count, 0);
        self.slots[idx as usize].kind = 0; // in-band FREE
        self.free.push(idx);
        self.live -= 1;
    }

    #[inline]
    pub fn get(&self, idx: u32) -> &Inode {
        &self.slots[idx as usize]
    }

    #[inline]
    pub fn get_mut(&mut self, idx: u32) -> &mut Inode {
        &mut self.slots[idx as usize]
    }

    /// Live inode count (accounting input).
    #[inline]
    pub fn live(&self) -> u32 {
        self.live
    }

    /// The monotonic ino counter (journal/snapshot header field — spec §8).
    #[inline]
    pub fn next_ino(&self) -> u64 {
        self.next_ino
    }

    /// Restore a snapshot's ino counter (persistence seam, P2-006).
    pub fn restore_next_ino(&mut self, v: u64) {
        self.next_ino = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inode_layout_is_frozen_72b() {
        assert_eq!(size_of::<Inode>(), 72);
        assert_eq!(core::mem::align_of::<Inode>(), 8);
        // Field offsets per spec §1.
        let i = Inode {
            ino: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            atime_ns: 0,
            birthtime_ns: 0,
            size: 0,
            nlink: 0,
            open_count: 0,
            data: 0,
            parent_idx: 0,
            perm: 0,
            kind: 1,
            dev: 1,
            flags: 0,
            _pad: [0; 3],
        };
        let base = &i as *const Inode as usize;
        assert_eq!((&i.ino as *const u64 as usize) - base, 0x00);
        assert_eq!((&i.mtime_ns as *const i64 as usize) - base, 0x08);
        assert_eq!((&i.size as *const u64 as usize) - base, 0x28);
        // nlink at 0x30, open_count 0x34, data 0x38, parent 0x3C
        let i = Inode {
            ino: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            atime_ns: 0,
            birthtime_ns: 0,
            size: 0,
            nlink: 0,
            open_count: 0,
            data: 0,
            parent_idx: 0,
            perm: 0,
            kind: 1,
            dev: 1,
            flags: 0,
            _pad: [0; 3],
        };
        let base = &i as *const Inode as usize;
        assert_eq!((&i.nlink as *const u32 as usize) - base, 0x30);
        assert_eq!((&i.open_count as *const u32 as usize) - base, 0x34);
        assert_eq!((&i.data as *const u32 as usize) - base, 0x38);
        assert_eq!((&i.parent_idx as *const u32 as usize) - base, 0x3C);
        assert_eq!((&i.perm as *const u16 as usize) - base, 0x40);
        assert_eq!((&i.kind as *const u8 as usize) - base, 0x42);
        assert_eq!((&i.dev as *const u8 as usize) - base, 0x43);
    }

    #[test]
    fn ino_never_reused_across_cycles() {
        let mut a = InodeArena::with_root(0, 1, 0o755);
        let mut seen = alloc::collections::BTreeSet::new();
        for _ in 0..10_000 {
            let idx = a.alloc(InodeKind::File, 0o644, 1, 0);
            let ino = a.get(idx).ino;
            assert!(seen.insert(ino), "ino {ino} reused");
            a.get_mut(idx).nlink = 0;
            a.get_mut(idx).open_count = 0;
            a.free(idx);
        }
        assert_eq!(a.live(), 1); // the root
    }

    #[test]
    fn root_has_nlink_two() {
        let a = InodeArena::with_root(0, 1, 0o755);
        assert!(a.get(0).is_dir());
        assert_eq!(a.get(0).nlink, 2);
        assert_eq!(a.get(0).ino, 1);
    }
}
