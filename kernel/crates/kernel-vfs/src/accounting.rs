//! Resource accounting + ceilings (spec §8; rulings 0-6, AV-4/AV-5).
//!
//! - `residency` = content bytes + boxed-name bytes + chunk bookkeeping.
//!   Pre-flight-checked against `residency_ceiling` (1536 MiB default, raised from 768) ⇒
//!   `ENOSPC` BEFORE any allocation (allocation failure itself would trap
//!   the kernel — panic=immediate-abort; never the error path).
//! - Metadata bytes (72 B/inode, 40 B/dirent, 8 B/dirent index) are tracked
//!   but charged to the trap-safety slack class, NOT residency (H §2.7).
//! - inode cap 2²⁰ ⇒ ENOSPC; the OFD cap lives in the fd layer
//!   (P2-005-005).
//! - O(1) `statfs` projection with the 8-field Node set (incl. `frsize` —
//!   utils.js:788-805).

use crate::{Errno, VResult};

pub const DEFAULT_RESIDENCY_CEILING: u64 = 1536 << 20; // raised from 768 MiB (ruling 0-6) — SDK-58 installs peak 724-744 MiB VFS residency and hit ENOSPC; KSM max raised to 2 GiB to match
pub const DEFAULT_PER_FILE_CEILING: u64 = 512 << 20; // EFBIG
pub const DEFAULT_INODE_CAP: u32 = 1 << 20;

/// `st_dev`-adjacent filesystem type magic for statfs (documented virtual
/// semantics — "CRVF").
pub const NODINE_VFS_MAGIC: u64 = 0x4352_5646;

pub const STATFS_BSIZE: u64 = 65_536;

/// The 8-field Node StatFs set (f_type, f_bsize, f_blocks, f_bfree, f_bavail,
/// f_files, f_ffree, f_frsize).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFsData {
    pub f_type: u64,
    pub f_bsize: u64,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: u64,
    pub f_files: u64,
    pub f_ffree: u64,
    pub f_frsize: u64,
}

#[derive(Debug, Clone)]
pub struct VfsAccounting {
    pub residency_ceiling: u64,
    pub per_file_ceiling: u64,
    pub inode_cap: u32,
    /// Live content bytes (logical sizes).
    pub content_bytes: u64,
    /// Chunk bookkeeping class (CHUNK_OVERHEAD_BYTES × chunks).
    pub chunk_overhead_bytes: u64,
    /// Boxed (>23 B) name bytes.
    pub name_bytes: u64,
    /// 72 B × live inodes (slack class — observed, not residency-charged).
    pub inode_bytes: u64,
    /// 40 B × live dirents (slack class).
    pub dirent_bytes: u64,
    /// 8 B × live dirents of index storage (slack class).
    pub dentry_index_bytes: u64,
    pub inodes_live: u32,
    pub dirents_live: u32,
}

impl Default for VfsAccounting {
    fn default() -> Self {
        VfsAccounting {
            residency_ceiling: DEFAULT_RESIDENCY_CEILING,
            per_file_ceiling: DEFAULT_PER_FILE_CEILING,
            inode_cap: DEFAULT_INODE_CAP,
            content_bytes: 0,
            chunk_overhead_bytes: 0,
            name_bytes: 0,
            inode_bytes: 0,
            dirent_bytes: 0,
            dentry_index_bytes: 0,
            inodes_live: 0,
            dirents_live: 0,
        }
    }
}

impl VfsAccounting {
    /// The ceiling-controlled quantity (spec §8): content + names + chunk
    /// overhead.
    pub fn residency(&self) -> u64 {
        self.content_bytes + self.name_bytes + self.chunk_overhead_bytes
    }

    pub fn can_charge(&self, content: u64, overhead: u64, names: u64) -> bool {
        self.residency()
            .checked_add(content + overhead + names)
            .map(|r| r <= self.residency_ceiling)
            .unwrap_or(false)
    }

    /// Pre-flight charge. On `Err` NOTHING has changed (bit-identical
    /// counters — acceptance A-7.9).
    pub fn charge(&mut self, content: u64, overhead: u64, names: u64) -> VResult<()> {
        if !self.can_charge(content, overhead, names) {
            return Err(Errno::Enospc);
        }
        self.content_bytes += content;
        self.chunk_overhead_bytes += overhead;
        self.name_bytes += names;
        Ok(())
    }

    /// Refund (saturating — never fails, never underflows).
    pub fn refund(&mut self, content: u64, overhead: u64, names: u64) {
        self.content_bytes = self.content_bytes.saturating_sub(content);
        self.chunk_overhead_bytes = self.chunk_overhead_bytes.saturating_sub(overhead);
        self.name_bytes = self.name_bytes.saturating_sub(names);
    }

    /// Inode allocation pre-flight (cap only — inode bytes are slack class).
    pub fn charge_inode(&mut self) -> VResult<()> {
        if self.inodes_live >= self.inode_cap {
            return Err(Errno::Enospc);
        }
        self.inodes_live += 1;
        self.inode_bytes += crate::INODE_SIZE as u64;
        Ok(())
    }

    pub fn refund_inode(&mut self) {
        self.inodes_live = self.inodes_live.saturating_sub(1);
        self.inode_bytes = self.inode_bytes.saturating_sub(crate::INODE_SIZE as u64);
    }

    /// Dirent charge (40 B slot + boxed-name bytes + 8 B of index storage —
    /// all slack class except names, which are residency).
    pub fn charge_dirent(&mut self, name_bytes: u64) -> VResult<()> {
        if !self.can_charge(0, 0, name_bytes) {
            return Err(Errno::Enospc);
        }
        self.dirents_live += 1;
        self.dirent_bytes += 40;
        self.dentry_index_bytes += 8;
        self.name_bytes += name_bytes;
        Ok(())
    }

    pub fn refund_dirent(&mut self, name_bytes: u64) {
        self.dirents_live = self.dirents_live.saturating_sub(1);
        self.dirent_bytes = self.dirent_bytes.saturating_sub(40);
        self.dentry_index_bytes = self.dentry_index_bytes.saturating_sub(8);
        self.name_bytes = self.name_bytes.saturating_sub(name_bytes);
    }

    /// Per-file EFBIG pre-flight.
    pub fn check_per_file(&self, new_size: u64) -> VResult<()> {
        if new_size > self.per_file_ceiling {
            return Err(Errno::Efbig);
        }
        Ok(())
    }

    /// O(1) statfs projection (spec §8).
    pub fn statfs(&self) -> StatFsData {
        let bsize = STATFS_BSIZE;
        let blocks = self.residency_ceiling / bsize;
        let bfree = self.residency_ceiling.saturating_sub(self.residency()) / bsize;
        StatFsData {
            f_type: NODINE_VFS_MAGIC,
            f_bsize: bsize,
            f_blocks: blocks,
            f_bfree: bfree,
            f_bavail: bfree,
            f_files: self.inode_cap as u64,
            f_ffree: (self.inode_cap - self.inodes_live) as u64,
            f_frsize: bsize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enospc_pre_flight_leaves_counters_bit_identical() {
        let mut a = VfsAccounting {
            residency_ceiling: 1000,
            ..Default::default()
        };
        a.charge(900, 0, 0).unwrap();
        let before = a.clone();
        assert_eq!(a.charge(200, 0, 0), Err(Errno::Enospc));
        assert_eq!(a.content_bytes, before.content_bytes);
        assert_eq!(a.residency(), before.residency());
        // Refund never underflows (saturates at 0).
        a.refund(500, 0, 0);
        a.refund(600, 0, 0);
        assert_eq!(a.content_bytes, 0, "refund saturates");
    }

    #[test]
    fn inode_cap_enforced() {
        let mut a = VfsAccounting {
            inode_cap: 2,
            ..Default::default()
        };
        a.charge_inode().unwrap();
        a.charge_inode().unwrap();
        assert_eq!(a.charge_inode(), Err(Errno::Enospc));
        a.refund_inode();
        a.charge_inode().unwrap();
    }

    #[test]
    fn statfs_projection_eight_fields() {
        let a = VfsAccounting {
            residency_ceiling: 768 << 20,
            ..Default::default()
        };
        let s = a.statfs();
        assert_eq!(s.f_bsize, 65536);
        assert_eq!(s.f_frsize, 65536);
        assert_eq!(s.f_blocks, (768 << 20) / 65536);
        assert_eq!(s.f_bfree, s.f_blocks);
        assert_eq!(s.f_files, 1 << 20);
        assert_eq!(s.f_ffree, 1 << 20);
        assert_eq!(s.f_type, NODINE_VFS_MAGIC);
    }
}
