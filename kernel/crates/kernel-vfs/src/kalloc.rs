//! The kernel heap allocator bring-up (ruling 0-8; B D5a — "no free-capable
//! allocator today" was P2-005's critical-path risk).
//!
//! Hand-rolled, `no_std`, zero-dependency, first-fit free-list allocator
//! with boundary-tag coalescing over a caller-provided region (the KSM heap
//! remainder in the wasm kernel; a plain buffer in host tests).
//!
//! Scope note (recorded honestly): ruling 0-8 names "segregated free-list"
//! as the technique; this initial bring-up implements the correctness-first
//! core (first-fit + full coalescing). Exact-size fast bins are the B8-gated
//! optimization pass — added only if the fragmentation soak demands them.
//!
//! ## Physical layout
//!
//! ```text
//! [state block (used)]  [block] [block] ... [tail free block]
//!  ^header at 0           ^headers at each block start
//! ```
//! - Block header (8 B): `size: u32` (total block bytes, 8-aligned,
//!   includes header+footer), `used: u32` (1 = used, 0 = free).
//! - Block footer (4 B): size mirror at `block_end − 4` (backward
//!   coalescing).
//! - Free-block body starts with the doubly-linked free-list pointers
//!   (`next`, `prev`, u32 region offsets; `u32::MAX` = null).
//! - State block content (at offset 8): magic, region_size, free-list head,
//!   alloc/free debug counters, live block bytes, high-water mark. All
//!   state lives INSIDE the region (offset-based u32 — the KSM is ≤ 1 GiB),
//!   so a `Kalloc` value is a stateless handle the kernel can re-attach
//!   from a fixed cell (no Rust statics — layout gate).
//!
//! ## Region growth (P2-005-004b)
//!
//! `grow(new_size)` extends the region into freshly-mapped memory (the
//! kernel grows the wasm linear memory first, then extends the allocator
//! region): the tail free block absorbs the extension, or — if the tail
//! block is used — a new free block is appended. `live()`/`high_water()`
//! are O(1) counter reads (bench + kernel diagnostics); `stats()` stays an
//! independent physical walk used to cross-check them.
//!
//! ## Over-aligned allocations
//!
//! `alloc_aligned(n, align)` serves `Layout::align() > 8` requests (the
//! common path stays the plain 8-aligned `alloc`). Over-aligned blocks
//! stash their block offset in the 4 bytes below the returned pointer, so
//! `dealloc_aligned` recovers the block without any side table.
//!
//! Safety contract: a `Kalloc` is bound to one exclusive 8-aligned region;
//! pointers from `alloc` are valid until the matching `dealloc`; double-free
//! and foreign-pointer frees are UB (kernel discipline — the VFS layer owns
//! the lifetimes).

#![allow(unsafe_code)] // raw-region allocator — see the contract above

/// Minimum region size.
pub const MIN_REGION: usize = 1024;

const ALIGN: usize = 8;
const HDR: usize = 8;
const FTR: usize = 4;
/// header + footer + two link words, 8-aligned.
const MIN_BLOCK: usize = 24;
const STATE_MAGIC: u32 = 0x_C4A4_0001;
const NULL: u32 = u32::MAX;

// State-block content offsets (block header occupies 0..8).
const S_MAGIC: usize = 8;
const S_SIZE: usize = 12;
const S_HEAD: usize = 16;
const S_ACOUNT: usize = 20;
const S_FCOUNT: usize = 24;
const S_USED: usize = 28; // live block bytes (headers+footers included)
const S_HIGHW: usize = 32; // high-water of S_USED
const STATE_TOTAL: usize = 40; // align_up(HDR + 28, 8)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KallocStats {
    pub region_size: usize,
    pub used_bytes: usize,
    pub free_bytes: usize,
    pub free_blocks: usize,
}

/// Stateless-region-handle allocator.
pub struct Kalloc {
    base: *mut u8,
    size: usize,
}

impl Kalloc {
    /// Initialize a fresh region (clobbers it).
    ///
    /// # Safety
    /// `base` must be 8-aligned; `base..base+size` valid and exclusive.
    pub unsafe fn init(base: *mut u8, size: usize) -> Result<Kalloc, ()> {
        if size < MIN_REGION || (base as usize) % ALIGN != 0 || size > u32::MAX as usize {
            return Err(());
        }
        let free_off = STATE_TOTAL;
        let free_size = (size - free_off) as u32;
        if free_size < MIN_BLOCK as u32 {
            return Err(());
        }
        unsafe {
            // State block (marked used so walks/coalescing skip it).
            write_u32(base, 0, STATE_TOTAL as u32);
            write_u32(base, 4, 1);
            write_u32(base, STATE_TOTAL - FTR, STATE_TOTAL as u32);
            // State content. The state block itself is a live used block, so
            // the O(1) counters start at STATE_TOTAL to match `stats()`'s
            // physical walk (which counts it).
            write_u32(base, S_MAGIC, STATE_MAGIC);
            write_u32(base, S_SIZE, size as u32);
            write_u32(base, S_HEAD, free_off as u32);
            write_u32(base, S_ACOUNT, 0);
            write_u32(base, S_FCOUNT, 0);
            write_u32(base, S_USED, STATE_TOTAL as u32);
            write_u32(base, S_HIGHW, STATE_TOTAL as u32);
            // One big free block.
            write_u32(base, free_off, free_size);
            write_u32(base, free_off + 4, 0);
            write_u32(base, free_off + HDR, NULL);
            write_u32(base, free_off + HDR + 4, NULL);
            write_u32(base, free_off + free_size as usize - FTR, free_size);
        }
        Ok(Kalloc { base, size })
    }

    /// Attach to an ALREADY-initialized region (handle duplication).
    ///
    /// # Safety
    /// Region must be a valid, uncorrupted `init` region.
    pub unsafe fn attach(base: *mut u8, size: usize) -> Result<Kalloc, ()> {
        if size < MIN_REGION || (base as usize) % ALIGN != 0 {
            return Err(());
        }
        if unsafe { read_u32(base, S_MAGIC) } != STATE_MAGIC
            || unsafe { read_u32(base, S_SIZE) } as usize != size
        {
            return Err(());
        }
        Ok(Kalloc { base, size })
    }

    /// Read the region size of an initialized region (for owners that track
    /// only the base pointer — e.g. the kernel's KH header cell). `None` if
    /// the magic is absent (not initialized / corrupted).
    ///
    /// # Safety
    /// `base` must point at readable memory of at least STATE_TOTAL bytes.
    pub unsafe fn region_size(base: *mut u8) -> Option<usize> {
        if unsafe { read_u32(base, S_MAGIC) } != STATE_MAGIC {
            return None;
        }
        Some(unsafe { read_u32(base, S_SIZE) } as usize)
    }

    /// Allocate `n` bytes (8-aligned payload). `None` on exhaustion.
    ///
    /// # Safety
    /// Region discipline per module docs.
    pub unsafe fn alloc(&mut self, n: usize) -> Option<*mut u8> {
        let need = align_up(n + HDR + FTR, ALIGN).max(MIN_BLOCK);
        if need > self.size {
            return None;
        }
        let mut cur = unsafe { read_u32(self.base, S_HEAD) };
        while cur != NULL {
            let off = cur as usize;
            let bsize = unsafe { read_u32(self.base, off) } as usize;
            if bsize >= need {
                return Some(unsafe { self.take_block(off, bsize, need) });
            }
            cur = unsafe { read_u32(self.base, off + HDR) };
        }
        None
    }

    /// Free a pointer from `alloc`.
    ///
    /// # Safety
    /// `p` must be a live allocation of THIS allocator (double-free is UB).
    pub unsafe fn dealloc(&mut self, p: *mut u8) {
        let off = (p as usize - self.base as usize) - HDR;
        unsafe { self.dealloc_at(off) };
    }

    /// Free by BLOCK offset (the `alloc_aligned` recovery path).
    ///
    /// # Safety
    /// `off` must be a live block start of THIS region (double-free is UB).
    pub unsafe fn dealloc_at(&mut self, off: usize) {
        let bsize = unsafe { read_u32(self.base, off) } as usize;
        debug_assert!(bsize >= MIN_BLOCK && off + bsize <= self.size);
        unsafe {
            write_u32(self.base, off + 4, 0);
            let used = read_u32(self.base, S_USED);
            write_u32(self.base, S_USED, used.saturating_sub(bsize as u32));
        };
        let mut start = off;
        let mut size = bsize;

        // Coalesce forward.
        let next = off + bsize;
        if next + HDR <= self.size && unsafe { read_u32(self.base, next + 4) } == 0 {
            let nsize = unsafe { read_u32(self.base, next) } as usize;
            unsafe { self.unlink_free(next) };
            size += nsize;
        }

        // Coalesce backward (footer of the previous physical block). The
        // state block is marked used, so `prev == 0` never coalesces.
        if start >= STATE_TOTAL + MIN_BLOCK {
            let prev_size = unsafe { read_u32(self.base, start - FTR) } as usize;
            let prev = start - prev_size;
            if prev >= STATE_TOTAL
                && prev_size >= MIN_BLOCK
                && unsafe { read_u32(self.base, prev + 4) } == 0
            {
                unsafe { self.unlink_free(prev) };
                start = prev;
                size += prev_size;
            }
        }

        unsafe {
            write_u32(self.base, start, size as u32);
            write_u32(self.base, start + 4, 0);
            write_u32(self.base, start + size - FTR, size as u32);
            self.push_free(start);
            bump(self.base, S_FCOUNT);
        }
    }

    /// Extend the region to `new_size` (the caller has already mapped the
    /// new memory — e.g. grew the wasm linear memory). The tail free block
    /// absorbs the extension; if the tail block is used, a new free block
    /// is appended. Fails (without touching state) if the extension cannot
    /// form a usable free block in that case.
    ///
    /// # Safety
    /// `[base + self.size, base + new_size)` must be valid, exclusive
    /// memory; `new_size > self.size`; region must be a live `init` region.
    pub unsafe fn grow(&mut self, new_size: usize) -> Result<(), ()> {
        if new_size <= self.size || new_size > u32::MAX as usize {
            return Err(());
        }
        let old = self.size;
        let extra = new_size - old;
        unsafe {
            let last_size = read_u32(self.base, old - FTR) as usize;
            if last_size < MIN_BLOCK || old < last_size {
                return Err(()); // corrupt tail footer
            }
            let last_off = old - last_size;
            let last_used = read_u32(self.base, last_off + 4) == 1;
            if !last_used {
                // Tail free block absorbs the extension.
                let big = (last_size + extra) as u32;
                write_u32(self.base, last_off, big);
                write_u32(self.base, last_off + 4, 0);
                write_u32(self.base, new_size - FTR, big);
            } else if extra >= MIN_BLOCK {
                // Append a fresh free block.
                let e = extra as u32;
                write_u32(self.base, old, e);
                write_u32(self.base, old + 4, 0);
                write_u32(self.base, old + HDR, NULL);
                write_u32(self.base, old + HDR + 4, NULL);
                write_u32(self.base, new_size - FTR, e);
                self.push_free(old);
            } else {
                return Err(()); // cannot form a block; caller must grow more
            }
            write_u32(self.base, S_SIZE, new_size as u32);
        }
        self.size = new_size;
        Ok(())
    }

    /// Allocate `n` bytes at `align` (> 8 supported; ≤ 8 takes the plain
    /// path). The 4 bytes below the returned pointer hold the block offset
    /// for `dealloc_aligned` — never visible to the caller.
    ///
    /// # Safety
    /// Region discipline per module docs.
    pub unsafe fn alloc_aligned(&mut self, n: usize, align: usize) -> Option<*mut u8> {
        if align <= ALIGN {
            return self.alloc(n);
        }
        if !align.is_power_of_two() {
            return None; // Layout guarantees power-of-two; defensive
        }
        let total = n.checked_add(align)?;
        let raw = unsafe { self.alloc(total)? } as usize;
        let mut p = align_up(raw, align);
        if p + 4 > raw + total || p - raw < 4 {
            p += align; // guarantee stash room strictly inside the block
        }
        debug_assert!(p >= raw + 4 && p + n <= raw + total);
        unsafe {
            // Stash the block offset at the absolute address p−4 (the
            // write_u32 helper takes a region-relative offset).
            write_u32(self.base, p - 4 - self.base as usize, (raw - self.base as usize - HDR) as u32);
        }
        Some(p as *mut u8)
    }

    /// Free a pointer from `alloc_aligned`.
    ///
    /// # Safety
    /// `p` must be a live `alloc_aligned` result of THIS allocator.
    pub unsafe fn dealloc_aligned(&mut self, p: *mut u8) {
        let rel = p as usize - self.base as usize;
        let off = unsafe { read_u32(self.base, rel - 4) } as usize;
        unsafe { self.dealloc_at(off) };
    }

    /// Live block bytes (O(1) counter — headers+footers included).
    pub fn live(&self) -> usize {
        unsafe { read_u32(self.base, S_USED) as usize }
    }

    /// High-water of `live()` (O(1) counter).
    pub fn high_water(&self) -> usize {
        unsafe { read_u32(self.base, S_HIGHW) as usize }
    }

    /// Walk physical blocks; sanity stats (host tests + kernel diagnostics).
    pub fn stats(&self) -> KallocStats {
        let mut used = 0usize;
        let mut free = 0usize;
        let mut blocks = 0usize;
        let mut cur = 0usize;
        while cur + HDR <= self.size {
            let sz = unsafe { read_u32(self.base, cur) } as usize;
            if sz < MIN_BLOCK || cur + sz > self.size {
                break; // corrupt region — report what we walked
            }
            if unsafe { read_u32(self.base, cur + 4) } == 1 {
                used += sz;
            } else {
                free += sz;
                blocks += 1;
            }
            cur += sz;
        }
        KallocStats {
            region_size: self.size,
            used_bytes: used,
            free_bytes: free,
            free_blocks: blocks,
        }
    }

    /// Debug counters.
    pub fn counters(&self) -> (u32, u32) {
        unsafe { (read_u32(self.base, S_ACOUNT), read_u32(self.base, S_FCOUNT)) }
    }

    // ---- internals -------------------------------------------------------

    unsafe fn take_block(&mut self, off: usize, bsize: usize, need: usize) -> *mut u8 {
        unsafe { self.unlink_free(off) };
        let mut size = bsize;
        if bsize - need >= MIN_BLOCK {
            let rem = off + need;
            let rem_size = bsize - need;
            unsafe {
                write_u32(self.base, rem, rem_size as u32);
                write_u32(self.base, rem + 4, 0);
                write_u32(self.base, rem + rem_size - FTR, rem_size as u32);
                self.push_free(rem);
            }
            size = need;
        }
        unsafe {
            write_u32(self.base, off, size as u32);
            write_u32(self.base, off + 4, 1);
            write_u32(self.base, off + size - FTR, size as u32);
            let used = read_u32(self.base, S_USED).saturating_add(size as u32);
            write_u32(self.base, S_USED, used);
            if used > read_u32(self.base, S_HIGHW) {
                write_u32(self.base, S_HIGHW, used);
            }
            bump(self.base, S_ACOUNT);
            self.base.add(off + HDR)
        }
    }

    unsafe fn push_free(&mut self, off: usize) {
        let head = unsafe { read_u32(self.base, S_HEAD) };
        unsafe {
            write_u32(self.base, off + HDR, head);
            write_u32(self.base, off + HDR + 4, NULL);
            if head != NULL {
                write_u32(self.base, head as usize + HDR + 4, off as u32);
            }
            write_u32(self.base, S_HEAD, off as u32);
        }
    }

    unsafe fn unlink_free(&mut self, off: usize) {
        let next = unsafe { read_u32(self.base, off + HDR) };
        let prev = unsafe { read_u32(self.base, off + HDR + 4) };
        unsafe {
            if next != NULL {
                write_u32(self.base, next as usize + HDR + 4, prev);
            }
            if prev != NULL {
                write_u32(self.base, prev as usize + HDR, next);
            } else {
                write_u32(self.base, S_HEAD, next);
            }
        }
    }
}

#[inline]
fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

#[inline]
unsafe fn read_u32(base: *mut u8, off: usize) -> u32 {
    unsafe { core::ptr::read_unaligned(base.add(off) as *const u32) }
}

#[inline]
unsafe fn write_u32(base: *mut u8, off: usize, v: u32) {
    unsafe { core::ptr::write_unaligned(base.add(off) as *mut u32, v) }
}

#[inline]
unsafe fn bump(base: *mut u8, off: usize) {
    unsafe { write_u32(base, off, read_u32(base, off).wrapping_add(1)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn with_region(size: usize) -> (Vec<u8>, Kalloc) {
        let mut region = vec![0u8; size];
        let base = region.as_mut_ptr();
        let k = unsafe { Kalloc::init(base, size).unwrap() };
        (region, k)
    }

    fn verify_pattern(p: *mut u8, n: usize, tag: u8) {
        for j in 0..n {
            assert_eq!(unsafe { *p.add(j) }, tag, "corruption at byte {j} (tag {tag})");
        }
    }

    #[test]
    fn alloc_free_roundtrip_and_coalesce() {
        let (_r, mut k) = with_region(64 * 1024);
        let a = unsafe { k.alloc(100).unwrap() };
        let b = unsafe { k.alloc(100).unwrap() };
        let c = unsafe { k.alloc(100).unwrap() };
        assert!(a != b && b != c);
        assert_eq!(k.stats().free_blocks, 1, "only the tail remainder is free");
        unsafe { k.dealloc(a) };
        unsafe { k.dealloc(c) };
        // c is adjacent to the tail remainder ⇒ they coalesce: a-hole +
        // (c+tail) = 2 free blocks.
        assert_eq!(k.stats().free_blocks, 2);
        unsafe { k.dealloc(b) };
        // a+b+c+tail all coalesce into one block.
        assert_eq!(k.stats().free_blocks, 1, "full coalescing");
        // A block spanning the old a..c hole now fits.
        let big = unsafe { k.alloc(280).unwrap() };
        unsafe { k.dealloc(big) };
        let (ac, fc) = k.counters();
        assert!(ac >= 4 && fc >= 4);
    }

    #[test]
    fn patterns_never_overlap_or_corrupt() {
        let (_r, mut k) = with_region(256 * 1024);
        let mut live: Vec<(*mut u8, usize, u8)> = Vec::new();
        let mut seed = 0x1234_5678u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for round in 0..8usize {
            for i in 0..128usize {
                let tag = ((round * 128 + i) % 256) as u8;
                let n = 8 + next() % 512;
                let p = unsafe { k.alloc(n).unwrap() };
                unsafe { core::ptr::write_bytes(p, tag, n) };
                live.push((p, n, tag));
            }
            // Verify every live allocation.
            for &(p, n, tag) in &live {
                verify_pattern(p, n, tag);
            }
            // Free every other allocation (alternating), then verify
            // survivors again.
            let mut i = 0usize;
            let mut keep: Vec<(*mut u8, usize, u8)> = Vec::new();
            for e in live.drain(..) {
                i += 1;
                if i % 2 == 0 {
                    unsafe { k.dealloc(e.0) };
                } else {
                    keep.push(e);
                }
            }
            live = keep;
            for &(p, n, tag) in &live {
                verify_pattern(p, n, tag);
            }
        }
        for &(p, n, tag) in &live {
            verify_pattern(p, n, tag);
        }
        for &(p, _, _) in &live {
            unsafe { k.dealloc(p) };
        }
        assert_eq!(k.stats().free_blocks, 1, "everything coalesces back");
    }

    #[test]
    fn exhaustion_returns_none() {
        let (_r, mut k) = with_region(4096);
        let mut ptrs = Vec::new();
        while let Some(p) = unsafe { k.alloc(512) } {
            ptrs.push(p);
        }
        assert!(!ptrs.is_empty());
        assert!(unsafe { k.alloc(512) }.is_none(), "exhaustion = None, not UB");
        for p in ptrs {
            unsafe { k.dealloc(p) };
        }
        assert!(unsafe { k.alloc(2048) }.is_some(), "region fully recycled");
    }

    #[test]
    fn fragmentation_soak_smoke() {
        // B8-shaped: create storm → free 50% → create again; survivors intact.
        let (_r, mut k) = with_region(1024 * 1024);
        let mut live: Vec<(*mut u8, usize, u8)> = Vec::new();
        for i in 0..2000usize {
            let n = 24 + (i % 11) * 8; // inode/dirent-shaped sizes
            let p = unsafe { k.alloc(n).unwrap() };
            unsafe { core::ptr::write_bytes(p, (i % 256) as u8, n) };
            live.push((p, n, (i % 256) as u8));
        }
        let mut keep = Vec::new();
        for (i, e) in live.into_iter().enumerate() {
            if i % 2 == 0 {
                keep.push(e);
            } else {
                unsafe { k.dealloc(e.0) };
            }
        }
        for j in 0..1000usize {
            let n = 24 + (j % 13) * 8;
            let p = unsafe { k.alloc(n).unwrap() };
            unsafe { core::ptr::write_bytes(p, 0xA5, n) };
            if j % 3 == 0 {
                unsafe { k.dealloc(p) };
            }
        }
        // Odd originals were freed; even survivors must be byte-identical.
        for (i, &(p, n, tag)) in keep.iter().enumerate() {
            assert_eq!(tag, ((i * 2) % 256) as u8);
            verify_pattern(p, n, tag);
        }
        let s = k.stats();
        assert_eq!(s.used_bytes + s.free_bytes, s.region_size);
    }

    #[test]
    fn attach_validates_region() {
        let mut region = vec![0u8; 4096];
        let base = region.as_mut_ptr();
        unsafe { Kalloc::init(base, 4096).unwrap() };
        assert!(unsafe { Kalloc::attach(base, 4096).is_ok() });
        assert!(unsafe { Kalloc::attach(base, 8192).is_err() });
        let mut junk = vec![0u8; 4096];
        assert!(unsafe { Kalloc::attach(junk.as_mut_ptr(), 4096).is_err() });
    }

    #[test]
    fn tiny_allocations_round_to_min_block() {
        let (_r, mut k) = with_region(4096);
        let p = unsafe { k.alloc(1).unwrap() };
        let q = unsafe { k.alloc(0).unwrap() };
        assert!(p != q);
        unsafe { k.dealloc(p) };
        unsafe { k.dealloc(q) };
        let s = k.stats();
        assert_eq!(s.used_bytes + s.free_bytes, s.region_size);
        assert_eq!(s.free_blocks, 1);
    }

    #[test]
    fn grow_extends_tail_free_block() {
        let mut region = vec![0u8; 64 * 1024];
        let base = region.as_mut_ptr();
        let mut k = unsafe { Kalloc::init(base, 32 * 1024).unwrap() };
        // Exhaust the 32 KiB region with survivors.
        let mut live = Vec::new();
        let mut i = 0usize;
        while let Some(p) = unsafe { k.alloc(256) } {
            unsafe { core::ptr::write_bytes(p, (i % 256) as u8, 256) };
            live.push((p, (i % 256) as u8));
            i += 1;
        }
        assert!(i >= 100, "region genuinely exhausted ({i} survivors)");
        assert!(unsafe { k.alloc(4096) }.is_none(), "exhausted before grow");
        let used_before = k.live();
        // Grow into the untouched upper half of the host buffer.
        unsafe { k.grow(64 * 1024).unwrap() };
        let q = unsafe { k.alloc(4096).unwrap() };
        unsafe { core::ptr::write_bytes(q, 0x77, 4096) };
        // Survivors intact across the growth.
        for (p, tag) in &live {
            for j in 0..256 {
                assert_eq!(unsafe { *p.add(j) }, *tag);
            }
        }
        unsafe { k.dealloc(q) };
        for (p, _) in live {
            unsafe { k.dealloc(p) };
        }
        let s = k.stats();
        assert_eq!(s.region_size, 64 * 1024);
        assert_eq!(s.free_blocks, 1, "full re-coalesce after grow+free");
        assert_eq!(s.used_bytes + s.free_bytes, s.region_size);
        assert_eq!(k.live(), s.used_bytes, "counter matches walk");
        assert!(k.high_water() >= used_before);
    }

    #[test]
    fn grow_appends_block_when_tail_used() {
        let mut region = vec![0u8; 128 * 1024];
        let base = region.as_mut_ptr();
        let mut k = unsafe { Kalloc::init(base, 32 * 1024).unwrap() };
        // Snap the tail block so the region ends USED.
        let a = unsafe { k.alloc(8 * 1024).unwrap() };
        unsafe { core::ptr::write_bytes(a, 1, 8 * 1024) };
        let b = unsafe { k.alloc(8 * 1024).unwrap() };
        unsafe { core::ptr::write_bytes(b, 2, 8 * 1024) };
        let rem = k.stats().free_bytes;
        assert!(rem >= MIN_BLOCK);
        let c = unsafe { k.alloc(rem - 12).unwrap() }; // exact remainder
        unsafe { core::ptr::write_bytes(c, 3, rem - 12) };
        assert_eq!(k.stats().free_bytes, 0, "tail is used");
        unsafe { k.grow(128 * 1024).unwrap() };
        let d = unsafe { k.alloc(64 * 1024).unwrap() }; // only fits in the extension
        unsafe { core::ptr::write_bytes(d, 0x5A, 64 * 1024) };
        for (p, n, t) in [(a, 8 * 1024usize, 1u8), (b, 8 * 1024, 2u8), (c, rem - 12, 3u8)] {
            for j in 0..n {
                assert_eq!(unsafe { *p.add(j) }, t);
            }
        }
        assert!(unsafe { Kalloc::attach(base, 128 * 1024) }.is_ok(), "S_SIZE updated");
        unsafe { k.dealloc(d) };
        unsafe { k.dealloc(c) };
        unsafe { k.dealloc(b) };
        unsafe { k.dealloc(a) };
        let s = k.stats();
        assert_eq!(s.free_blocks, 1);
        assert_eq!(s.used_bytes + s.free_bytes, s.region_size);
    }

    #[test]
    fn grow_rejects_shrink_and_overflow() {
        let (_r, mut k) = with_region(8192);
        assert!(unsafe { k.grow(8192) }.is_err());
        assert!(unsafe { k.grow(4096) }.is_err());
        assert!(unsafe { k.grow(u32::MAX as usize + 1) }.is_err());
    }

    #[test]
    fn aligned_allocations_honor_align_and_roundtrip() {
        let (_r, mut k) = with_region(256 * 1024);
        for align in [16usize, 32, 64, 4096] {
            for n in [1usize, 24, 100, 1000] {
                let p = unsafe { k.alloc_aligned(n, align).unwrap() };
                assert_eq!(p as usize % align, 0, "align {align} n {n}");
                unsafe { core::ptr::write_bytes(p, 0xA7, n) };
                unsafe { k.dealloc_aligned(p) };
            }
        }
        // Mixed with plain allocations; full accounting round-trip.
        let plain = unsafe { k.alloc(100).unwrap() };
        let al = unsafe { k.alloc_aligned(100, 64).unwrap() };
        assert_eq!(al as usize % 64, 0);
        unsafe { core::ptr::write_bytes(al, 0xB9, 100) };
        for j in 0..100 {
            assert_eq!(unsafe { *al.add(j) }, 0xB9);
        }
        unsafe { k.dealloc(plain) };
        unsafe { k.dealloc_aligned(al) };
        let s = k.stats();
        assert_eq!(s.used_bytes + s.free_bytes, s.region_size);
        assert_eq!(s.free_blocks, 1, "no leaks from aligned path");
        assert_eq!(k.live(), s.used_bytes);
    }

    #[test]
    fn live_and_high_water_counters_track_the_walk() {
        let (_r, mut k) = with_region(64 * 1024);
        assert_eq!(k.live(), STATE_TOTAL, "state block only");
        assert_eq!(k.high_water(), STATE_TOTAL);
        let mut ptrs = Vec::new();
        for i in 0..32usize {
            let p = unsafe { k.alloc(1024).unwrap() };
            unsafe { core::ptr::write_bytes(p, i as u8, 1024) };
            ptrs.push(p);
            assert_eq!(k.high_water(), k.stats().used_bytes);
            assert_eq!(k.live(), k.stats().used_bytes);
        }
        assert!(k.high_water() >= 32 * 1024, "monotone high-water");
        for p in ptrs {
            unsafe { k.dealloc(p) };
            assert_eq!(k.live(), k.stats().used_bytes, "counter matches walk after free");
        }
        assert_eq!(k.live(), STATE_TOTAL, "only the state block remains");
        assert!(k.high_water() > STATE_TOTAL, "high-water survives the drain");
        assert_eq!(k.stats().free_blocks, 1);
    }
}
