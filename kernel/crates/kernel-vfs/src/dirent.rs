//! Directory representation: SSO dirents + dual index (spec §5, rulings
//! 0-1/0-2/0-3).
//!
//! - Lookup structure: `by_name` — slot indices sorted by raw name bytes
//!   (binary search, no hash ⇒ no collision-DoS surface).
//! - Enumeration structure: `by_seq` — slot indices sorted by the global
//!   monotonic dentry sequence number (append keeps it sorted). This is the
//!   readdir cookie domain (R2): batch = live entries with `seq > cookie`,
//!   ascending; never misses an unmutated pre-existing entry, never repeats
//!   a dentry, insertions always appear after the cursor, hostile cookies
//!   (`u64::MAX`) are total (⇒ EOF).
//! - Frozen slot size: 40 B (= B's 32–36 B SSO + 8 B seq, H §3.1); names
//!   ≤ 23 B inline (zero alloc), 24–255 B boxed in the side slab.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem::size_of;

use crate::{Errno, VResult, NAME_MAX};

/// Longest name stored inline in the slot (B7-gated threshold; spec §5).
pub const DIRENT_NAME_INLINE_MAX: usize = 23;

/// Frozen dirent slot (40 B, align 8).
#[derive(Debug, Clone)]
#[repr(C, align(8))]
pub struct DirSlot {
    /// Global monotonic dentry sequence number — the readdir cookie domain.
    pub seq: u64,
    /// Target inode arena index.
    pub ino_idx: u32,
    /// 0 = name inline; else index+1 into `long_names`.
    name_ref: u32,
    /// Name byte length (≤ NAME_MAX).
    name_len: u8,
    inline_name: [u8; DIRENT_NAME_INLINE_MAX],
}

const _: () = assert!(size_of::<DirSlot>() == 40);

#[derive(Debug)]
#[allow(dead_code)] // fields kept for API completeness (kernel-core dispatch uses them)
pub(crate) struct InsertedDirent {
    pub slot: u32,
    /// Bytes charged for the name beyond the slot itself (0 inline).
    pub name_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // fields kept for API completeness (kernel-core dispatch uses them)
pub(crate) struct RemovedDirent {
    pub ino_idx: u32,
    pub seq: u64,
    /// Name bytes to refund (0 if the name was inline).
    pub name_bytes: usize,
}

/// One directory's children: stable slot slab + the two orderings.
///
/// Slot indices are stable across inserts/removes of OTHER entries (holes go
/// to the free list), which is what lets `by_name`/`by_seq` hold plain `u32`
/// indices that only move via their own memmoves — 4 B/entry, 8× cheaper
/// than moving whole dirent slots (B §4.3 tension resolved by the slab).
#[derive(Default)]
pub struct DirChildren {
    slots: Vec<DirSlot>,
    free: Vec<u32>,
    /// Slot indices sorted by name bytes (lookup path).
    by_name: Vec<u32>,
    /// Slot indices sorted by seq (enumeration path; append-sorted).
    by_seq: Vec<u32>,
    long_names: Vec<Option<Box<[u8]>>>,
    long_free: Vec<u32>,
}

impl DirChildren {
    pub fn new() -> DirChildren {
        DirChildren::default()
    }

    /// The name bytes of a live slot (borrowed from the slot or the side
    /// slab — valid until the next mutation of this directory).
    #[inline]
    pub fn name_of(&self, slot: u32) -> &[u8] {
        let s = &self.slots[slot as usize];
        if s.name_ref == 0 {
            &s.inline_name[..s.name_len as usize]
        } else {
            self.long_names[s.name_ref as usize - 1]
                .as_ref()
                .expect("live slot with dangling name_ref")
        }
    }

    /// Kind hint of a slot (from its target inode) — provided by the caller
    /// to avoid an arena back-reference here.
    #[inline]
    pub fn ino_of(&self, slot: u32) -> u32 {
        self.slots[slot as usize].ino_idx
    }

    #[inline]
    pub fn seq_of(&self, slot: u32) -> u64 {
        self.slots[slot as usize].seq
    }

    /// Binary-search `by_name` for `name`. Returns the insertion position
    /// and whether an exact match exists at it.
    fn seek_name(&self, name: &[u8]) -> (usize, bool) {
        let mut lo = 0usize;
        let mut hi = self.by_name.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let mid_name = self.name_of(self.by_name[mid]);
            match mid_name.cmp(name) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return (mid, true),
            }
        }
        (lo, false)
    }

    /// Lookup by exact name. Returns the slot index.
    pub fn lookup(&self, name: &[u8]) -> Option<u32> {
        let (pos, found) = self.seek_name(name);
        if found {
            Some(self.by_name[pos])
        } else {
            None
        }
    }

    pub fn contains(&self, name: &[u8]) -> bool {
        self.lookup(name).is_some()
    }

    /// Insert `name -> ino_idx` with the next global `seq`. `EEXIST` on a
    /// duplicate, `ENAMETOOLONG` past NAME_MAX. Charge hint in the result
    /// (spec §8 accounting).
    pub(crate) fn insert(&mut self, name: &[u8], ino_idx: u32, seq: u64) -> VResult<InsertedDirent> {
        if name.is_empty() {
            return Err(Errno::Einval);
        }
        if name.len() > NAME_MAX {
            return Err(Errno::Enametoolong);
        }
        let (pos, dup) = self.seek_name(name);
        if dup {
            return Err(Errno::Eexist);
        }

        let name_ref;
        let name_bytes;
        if name.len() <= DIRENT_NAME_INLINE_MAX {
            name_ref = 0;
            name_bytes = 0;
        } else {
            let idx = match self.long_free.pop() {
                Some(i) => {
                    self.long_names[i as usize] = Some(Box::from(name));
                    i
                }
                None => {
                    crate::grow_125(&mut self.long_names, 4);
                    self.long_names.push(Some(Box::from(name)));
                    (self.long_names.len() - 1) as u32
                }
            };
            name_ref = idx + 1;
            name_bytes = name.len();
        }

        let mut inline_name = [0u8; DIRENT_NAME_INLINE_MAX];
        if name_ref == 0 {
            inline_name[..name.len()].copy_from_slice(name);
        }
        let slot = match self.free.pop() {
            Some(i) => {
                self.slots[i as usize] = DirSlot {
                    seq,
                    ino_idx,
                    name_ref,
                    name_len: name.len() as u8,
                    inline_name,
                };
                i
            }
            None => {
                crate::grow_125(&mut self.slots, 4);
                self.slots.push(DirSlot {
                    seq,
                    ino_idx,
                    name_ref,
                    name_len: name.len() as u8,
                    inline_name,
                });
                (self.slots.len() - 1) as u32
            }
        };
        crate::grow_125(&mut self.by_name, 8); // 1.25x, not insert's 2x
        self.by_name.insert(pos, slot);
        crate::grow_125(&mut self.by_seq, 8);
        self.by_seq.push(slot); // seq monotonic ⇒ append keeps order
        Ok(InsertedDirent { slot, name_bytes })
    }

    /// Remove `name`. Frees the boxed name (refund hint in the result).
    pub(crate) fn remove(&mut self, name: &[u8]) -> Option<RemovedDirent> {
        let (pos, found) = self.seek_name(name);
        if !found {
            return None;
        }
        let slot = self.by_name.remove(pos);
        let s = &self.slots[slot as usize];
        let ino_idx = s.ino_idx;
        let seq = s.seq;
        let name_ref = s.name_ref;
        let name_len = s.name_len as usize;
        let mut name_bytes = 0usize;
        if name_ref != 0 {
            self.long_names[name_ref as usize - 1] = None;
            self.long_free.push(name_ref - 1);
            name_bytes = name_len;
        }
        // Drop from by_seq (binary search by unique seq).
        let seq_pos = self.by_seq.partition_point(|&sl| self.slots[sl as usize].seq < seq);
        debug_assert_eq!(
            self.by_seq.get(seq_pos).copied(),
            Some(slot),
            "by_seq invariant broken"
        );
        self.by_seq.remove(seq_pos);
        self.free.push(slot);
        Some(RemovedDirent {
            ino_idx,
            seq,
            name_bytes,
        })
    }

    /// Live entry count.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Enumerate live entries with `seq > cookie`, ascending by seq
    /// (creation order — ruling 0-2). Stops at `max_entries`; enforces
    /// `max_bytes` of cumulative name bytes but ALWAYS emits at least one
    /// entry when one is available and `max_entries ≥ 1` (progress
    /// guarantee — a too-small budget must never masquerade as EOF).
    ///
    /// Returns `(next_cookie, emitted)`: `next_cookie` is the last emitted
    /// seq (cookie for the following batch); `emitted == 0` ⇒ EOF.
    /// `max_entries == 0` is a legal EOF probe (F open question ruling).
    pub fn enumerate<F: FnMut(&[u8], u32, u64)>(
        &self,
        cookie: u64,
        max_entries: usize,
        max_bytes: usize,
        mut f: F,
    ) -> (u64, usize) {
        if max_entries == 0 {
            return (cookie, 0);
        }
        // First live entry with seq > cookie.
        let mut pos = self
            .by_seq
            .partition_point(|&sl| self.slots[sl as usize].seq <= cookie);
        let mut next_cookie = cookie;
        let mut emitted = 0usize;
        let mut bytes = 0usize;
        while emitted < max_entries && pos < self.by_seq.len() {
            let slot = self.by_seq[pos];
            let name = self.name_of(slot);
            if emitted > 0 && bytes + name.len() > max_bytes {
                break;
            }
            let s = &self.slots[slot as usize];
            f(name, s.ino_idx, s.seq);
            bytes += name.len();
            next_cookie = s.seq;
            emitted += 1;
            pos += 1;
        }
        (next_cookie, emitted)
    }

    /// Iterate all live entries in name order (tests, journal
    /// serialization order — spec §0 ruling 0-2).
    pub fn iter_by_name<F: FnMut(&[u8], u32)>(&self, mut f: F) {
        for &slot in &self.by_name {
            let s = &self.slots[slot as usize];
            f(self.name_of(slot), s.ino_idx);
        }
    }

    /// Byte charge of this directory's indexes (accounting class
    /// `dentry_index_bytes` — spec §8, AV-5 reconciliation).
    pub fn index_bytes(&self) -> u64 {
        (self.by_name.len() + self.by_seq.len()) as u64 * 4
    }

    /// Slot bytes (40 B × capacity class — charged on live entries).
    pub fn slot_bytes(&self) -> u64 {
        self.by_name.len() as u64 * 40
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins(d: &mut DirChildren, name: &str, ino: u32, seq: u64) {
        d.insert(name.as_bytes(), ino, seq).unwrap();
    }

    #[test]
    fn slot_layout_is_frozen_40b() {
        assert_eq!(size_of::<DirSlot>(), 40);
    }

    #[test]
    fn lookup_insert_remove_roundtrip() {
        let mut d = DirChildren::new();
        ins(&mut d, "package.json", 7, 1);
        ins(&mut d, "node_modules", 8, 2);
        ins(&mut d, "index.js", 9, 3);
        assert_eq!(d.lookup(b"package.json").map(|s| d.ino_of(s)), Some(7));
        assert_eq!(d.lookup(b"node_modules").map(|s| d.ino_of(s)), Some(8));
        assert!(d.lookup(b"missing").is_none());
        assert_eq!(d.len(), 3);
        let r = d.remove(b"node_modules").unwrap();
        assert_eq!(r.ino_idx, 8);
        assert_eq!(r.name_bytes, 0); // inline name
        assert!(d.lookup(b"node_modules").is_none());
        assert_eq!(d.len(), 2);
        // Duplicate rejected.
        assert_eq!(d.insert(b"index.js", 1, 4).unwrap_err(), Errno::Eexist);
    }

    #[test]
    fn long_names_box_and_refund() {
        let mut d = DirChildren::new();
        let long = "a-very-long-package-name-exceeding-inline@1.2.3-beta.1.tgz"; // >23
        assert!(long.len() > DIRENT_NAME_INLINE_MAX);
        ins(&mut d, long, 42, 1);
        let slot = d.lookup(long.as_bytes()).unwrap();
        assert_eq!(d.name_of(slot), long.as_bytes());
        let r = d.remove(long.as_bytes()).unwrap();
        assert_eq!(r.name_bytes, long.len());
        // Slot + long-name slot both recycled on re-insert.
        ins(&mut d, long, 43, 2);
        assert_eq!(d.lookup(long.as_bytes()).map(|s| d.ino_of(s)), Some(43));
    }

    #[test]
    fn name_order_is_byte_lexicographic() {
        let mut d = DirChildren::new();
        // Insert deliberately out of order.
        ins(&mut d, "zz", 1, 5);
        ins(&mut d, "aa", 2, 4);
        ins(&mut d, "mm", 3, 3);
        let mut names = Vec::new();
        d.iter_by_name(|n, _| names.push(n.to_vec()));
        assert_eq!(names, vec![b"aa".to_vec(), b"mm".to_vec(), b"zz".to_vec()]);
    }

    #[test]
    fn enumerate_is_creation_order_with_seq_cookies() {
        let mut d = DirChildren::new();
        ins(&mut d, "zz", 1, 10); // created first
        ins(&mut d, "aa", 2, 11);
        ins(&mut d, "mm", 3, 12);
        // Full scan.
        let mut out = Vec::new();
        let (nc, n) = d.enumerate(0, 100, 1 << 20, |name, ino, _seq| {
            out.push((name.to_vec(), ino))
        });
        assert_eq!(n, 3);
        assert_eq!(nc, 12);
        assert_eq!(out[0], (b"zz".to_vec(), 1)); // creation order, not name order
        assert_eq!(out[1], (b"aa".to_vec(), 2));
        // Resume from cookie 10 → skip the first.
        let mut out2 = Vec::new();
        let (_, n2) = d.enumerate(nc - 2, 100, 1 << 20, |name, ino, _| {
            out2.push((name.to_vec(), ino))
        });
        assert_eq!(n2, 2);
        assert_eq!(out2[0], (b"aa".to_vec(), 2));
        // EOF: cookie at/after the last seq.
        let (_, n3) = d.enumerate(12, 100, 1 << 20, |_, _, _| {});
        assert_eq!(n3, 0);
        // Hostile cookie is total (⇒ EOF), never an error.
        let (_, n4) = d.enumerate(u64::MAX, 100, 1 << 20, |_, _, _| {});
        assert_eq!(n4, 0);
        // max_entries = 0 is a legal EOF probe.
        let (_, n5) = d.enumerate(0, 0, 1 << 20, |_, _, _| {});
        assert_eq!(n5, 0);
    }

    #[test]
    fn enumeration_never_misses_or_repeats_under_mutation() {
        // R2 cookie contract (ruling 0-1; adversarial AV-2 scenario):
        // insert-before-cursor must not skip never-read entries.
        let mut d = DirChildren::new();
        for i in 0..100 {
            ins(&mut d, &format!("f{i:03}"), i as u32 + 1, i as u64 + 1);
        }
        // Read one batch of 10.
        let (cookie, n) = d.enumerate(0, 10, 1 << 20, |_, _, _| {});
        assert_eq!(n, 10);
        // Mutate: insert entries that sort BEFORE names already read.
        for i in 0..50 {
            ins(&mut d, &format!("a{i:03}"), 1000 + i, 1000 + i as u64);
        }
        // Remove some unread entries.
        for i in 90..100 {
            d.remove(format!("f{i:03}").as_bytes()).unwrap();
        }
        // Continue iterating: every remaining original entry with seq >
        // cookie must appear exactly once; all new ones too.
        let mut seen: Vec<u64> = Vec::new();
        let mut c = cookie;
        loop {
            let (nc, n) = d.enumerate(c, 7, 1 << 20, |_, _, seq| seen.push(seq));
            if n == 0 {
                break;
            }
            c = nc;
        }
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "duplicate seq emitted");
        // Originals 11..=90 survived (91.. wait: f011..f089 = 79 entries) + 50 new.
        let expect = (11..=90).count() + 50;
        assert_eq!(seen.len(), expect);
        // Every emitted seq is unique and > the initial cookie.
        assert!(seen.iter().all(|&s| s > cookie));
    }

    #[test]
    fn max_bytes_still_emits_one_entry_progress() {
        let mut d = DirChildren::new();
        ins(&mut d, "aaaaaaaaaaaaaaaaaaaaaaaaaa", 1, 1); // 26 B (boxed)
        let (_, n) = d.enumerate(0, 10, 1, |_, _, _| {});
        assert_eq!(n, 1, "progress guarantee: at least one entry");
    }

    #[test]
    fn nametoolong_rejected() {
        let mut d = DirChildren::new();
        let long = vec![b'x'; NAME_MAX + 1];
        assert_eq!(
            d.insert(&long, 1, 1).unwrap_err(),
            Errno::Enametoolong
        );
    }
}
