//! Persistence boundary: the observer seam (spec §8; G's design, ruling
//! via H).
//!
//! The VFS is the sole active source of truth; persistence (P2-006+) is a
//! CONSUMER of committed mutations. A mutation is committed when its op's
//! completion is published (kernel-core choke points) — the events below
//! carry **external ino identity** + `(parent_ino, name)` breadcrumbs (paths
//! are unstable under rename; ruling 0-7). When no observer is installed
//! the seam costs one branch — zero events, zero allocation.

/// Per-mutation metadata (the persistence seq spine — gap-free per kernel).
#[derive(Debug, Clone, Copy)]
pub struct MutationHeader {
    /// Gap-free kernel mutation sequence (journal idempotency key domain).
    pub seq: u64,
    /// Owning process (0 = host/kernel-internal).
    pub pid: u32,
    /// The ESM/SW generation counter (spec §1) after this mutation.
    pub epoch: u64,
}

/// One committed mutation. 9 variants (spec §8).
#[derive(Debug, Clone)]
pub enum MutationEvent<'a> {
    /// File created (open with O_CREAT on a new name).
    Create { ino: u64, parent_ino: u64, name: &'a [u8] },
    /// Symlink created.
    SymlinkCreate { ino: u64, parent_ino: u64, name: &'a [u8] },
    /// Byte range written (`pos..pos+len`); content deltas via accounting.
    Write { ino: u64, pos: u64, len: u64 },
    /// Truncate (old → new logical size).
    Truncate { ino: u64, old_size: u64, new_size: u64 },
    /// Name removed (`replaced` set when a rename atomically replaced it).
    Unlink { ino: u64, parent_ino: u64, name: &'a [u8], replaced: bool },
    /// Atomic rename (carries the replaced inode, if any).
    Rename {
        ino: u64,
        old_parent_ino: u64,
        old_name: &'a [u8],
        new_parent_ino: u64,
        new_name: &'a [u8],
        replaced_ino: Option<u64>,
    },
    /// Hardlink added.
    Link { ino: u64, parent_ino: u64, name: &'a [u8] },
    /// chmod.
    SetMode { ino: u64, perm: u16 },
    /// utimes/lutimes (any metadata-time change).
    SetTimes { ino: u64 },
}

pub trait VfsObserver {
    fn on_commit(&mut self, hdr: &MutationHeader, ev: &MutationEvent);
}
