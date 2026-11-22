//! # kernel-vfs — the Carapace in-memory virtual filesystem core (P2-005)
//!
//! Frozen architecture: `docs/phase2/P2_005_VFS_SPEC.md` (P2-005-003).
//! This crate is the single source of truth for active filesystem state.
//!
//! Design invariants (spec cross-references in each module):
//! - ONE VFS, no fallbacks, no JS filesystem — this crate is it.
//! - Single-threaded: no locks anywhere; ops run to completion (spec §9 R1).
//! - Identity: global monotonic u64 `ino`, never reused (spec §1).
//! - Directories: dual-index (name-sorted slots for lookup + seq-ordered
//!   index for enumeration); readdir cookies = stable dentry sequence
//!   numbers (spec §5, R2).
//! - File data: 2-tier + null (`Empty` / `Flat ≤ 256 KiB` / `64 KiB`
//!   chunks); no sparse files — holes are zero bytes (spec §4).
//! - Resource ceilings are PRE-FLIGHT checked; allocation failure itself is
//!   never the error path (spec §4/§8 — trap-on-OOM discipline).
//! - The persistence seam (`observer`) is a consumer of committed mutations,
//!   zero-cost when absent (spec §8).
//!
//! The crate is `no_std` + `alloc` in the kernel build; on the host it is
//! tested with the standard allocator (wasm builds install the KSM
//! segregated allocator as `#[global_allocator]` in kernel-core).

#![cfg_attr(not(test), no_std)]
// `deny` (not `forbid`): the allocator module (kalloc) is deliberately
// unsafe — raw-region memory management — and overrides it locally with a
// documented safety contract. Everything else in the crate stays safe.
#![deny(unsafe_code)]

extern crate alloc;

pub mod accounting;
pub mod content;
pub mod dirent;
pub mod fd;
pub mod inode;
pub mod kalloc;
pub mod observer;
pub mod path;
pub mod vfs;

pub use accounting::{StatFsData, VfsAccounting};
pub use content::{FileContent, CHUNK_SIZE, FLAT_MAX};
pub use dirent::{DirChildren, DirSlot, DIRENT_NAME_INLINE_MAX};
pub use fd::{
    FdCaps, FdCounters, FdState, FdTable, OpenFile, ProcessRecord, ACC_RDWR, ACC_RDONLY,
    ACC_WRONLY, FD_DEFAULT_UMASK, FD_MAX_PER_PROC, FD_MAX_PROCESSES, KIND_DEV_STDERR,
    KIND_DEV_STDIN, KIND_DEV_STDOUT, KIND_DIR, KIND_FILE, OFD_MAX_GLOBAL, STDIO_FIFO_CAP,
};
pub use inode::{Inode, InodeArena, InodeKind, INODE_SIZE};
pub use observer::{MutationEvent, MutationHeader, VfsObserver};
pub use path::SYMLOOP_MAX;
pub use vfs::{OpenModes, RawDirent, StatData, Vfs};

/// Filesystem limits frozen by the spec (§2) and the ABI crate.
pub const NAME_MAX: usize = 255;
pub const PATH_MAX: usize = 4096;

/// VFS operation error. Carries the Linux/libuv errno value
/// (`poc/p1-poc/uv-errno-linux.txt` ground truth); the ABI dispatch layer in
/// kernel-core renders `code()` as the negative completion status (ABI §14).
///
/// Frozen rulings (spec §7): `EXDEV` and `EROFS` are NOT in the set — one
/// tree ⇒ `link()`/`rename()` never cross devices; no read-only mounts in
/// Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    Eperm,        // 1
    Enoent,       // 2
    Ebadf,        // 9
    Ebusy,        // 16
    Eexist,       // 17
    Enotdir,      // 20
    Eisdir,       // 21
    Einval,       // 22
    Eagain,       // 11 (resource unavailable, would block — stdin/pipe class)
    Enfile,       // 23
    Emfile,       // 24
    Efbig,        // 27
    Enospc,       // 28
    Emlink,       // 31
    Enametoolong, // 36
    Enosys,       // 38
    Enotempty,    // 39
    Eloop,        // 40
    Eopnotsupp,   // 95
}

impl Errno {
    /// Positive errno number (Linux/libuv table).
    pub const fn number(self) -> i32 {
        match self {
            Errno::Eperm => 1,
            Errno::Enoent => 2,
            Errno::Ebadf => 9,
            Errno::Ebusy => 16,
            Errno::Eexist => 17,
            Errno::Enotdir => 20,
            Errno::Eisdir => 21,
            Errno::Einval => 22,
            Errno::Eagain => 11,
            Errno::Enfile => 23,
            Errno::Emfile => 24,
            Errno::Efbig => 27,
            Errno::Enospc => 28,
            Errno::Emlink => 31,
            Errno::Enametoolong => 36,
            Errno::Enosys => 38,
            Errno::Enotempty => 39,
            Errno::Eloop => 40,
            Errno::Eopnotsupp => 95,
        }
    }

    /// ABI completion `status` (negative errno — ABI §14).
    pub const fn code(self) -> i32 {
        -self.number()
    }
}

pub type VResult<T> = Result<T, Errno>;

/// Geometric 1.25x capacity growth (frozen D1a: "slot arena ... 1.25x
/// growth" — bounds capacity slack to ~25% worst-case / ~11% average,
/// versus Vec's built-in 2x doubling). `first_cap` is the first allocation
/// size for an empty vector.
#[inline]
pub(crate) fn grow_125<T>(v: &mut alloc::vec::Vec<T>, first_cap: usize) {
    if v.len() == v.capacity() {
        let cap = v.capacity();
        let new_cap = if cap == 0 {
            first_cap
        } else {
            cap + cap.div_ceil(4)
        };
        v.reserve_exact(new_cap - cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_values_match_libuv_linux_table() {
        // Ground truth: poc/p1-poc/uv-errno-linux.txt
        assert_eq!(Errno::Eperm.number(), 1);
        assert_eq!(Errno::Enoent.number(), 2);
        assert_eq!(Errno::Ebadf.number(), 9);
        assert_eq!(Errno::Ebusy.number(), 16);
        assert_eq!(Errno::Eexist.number(), 17);
        assert_eq!(Errno::Enotdir.number(), 20);
        assert_eq!(Errno::Eisdir.number(), 21);
        assert_eq!(Errno::Einval.number(), 22);
        assert_eq!(Errno::Enfile.number(), 23);
        assert_eq!(Errno::Emfile.number(), 24);
        assert_eq!(Errno::Efbig.number(), 27);
        assert_eq!(Errno::Enospc.number(), 28);
        assert_eq!(Errno::Emlink.number(), 31);
        assert_eq!(Errno::Enametoolong.number(), 36);
        assert_eq!(Errno::Enosys.number(), 38);
        assert_eq!(Errno::Enotempty.number(), 39);
        assert_eq!(Errno::Eloop.number(), 40);
        assert_eq!(Errno::Eopnotsupp.number(), 95);
        // ABI status is the negative errno.
        assert_eq!(Errno::Enoent.code(), -2);
    }
}
