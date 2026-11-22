//! File content storage — 2-tier + null (spec §4, ruling via B D4).
//!
//! - `Empty` (0 B, zero allocation — the keep-file win)
//! - `Flat` ≤ 256 KiB (one exact-ish allocation; a file written in ≤ 1 op
//!   never migrates)
//! - `Chunked` above: 64 KiB chunks (1 wasm page; ≤ 5 chunks per 256 KiB
//!   transport payload), append-only growth, no realloc-copies
//!
//! No sparse files: holes are real zero bytes (memset on gap-fill — recycled
//! chunks are dirty). All deltas are reported so the VFS can pre-flight the
//! residency ceiling BEFORE mutating (spec §4: allocation failure is never
//! the error path).

use alloc::boxed::Box;
use alloc::vec::Vec;

pub const FLAT_MAX: u64 = 256 * 1024;
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Documented per-chunk bookkeeping class charged to the accounting
/// `chunk_overhead` counter (spec §8).
pub const CHUNK_OVERHEAD_BYTES: u64 = 16;

/// A content-size delta (bytes above/below logical size, plus the chunk
/// bookkeeping class). Negative values are refunds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentDelta {
    pub content: i64,
    pub overhead: i64,
}

impl ContentDelta {
    pub const ZERO: ContentDelta = ContentDelta { content: 0, overhead: 0 };
}

#[derive(Debug, Clone, Default)]
pub enum FileContent {
    #[default]
    Empty,
    Flat(Vec<u8>),
    /// `tail_len` = byte length of the last chunk; earlier chunks are full
    /// `CHUNK_SIZE`.
    Chunked { tail_len: u32, chunks: Vec<Box<[u8]>> },
}

#[inline]
fn chunk_index(pos: u64) -> (usize, usize) {
    ((pos / CHUNK_SIZE as u64) as usize, (pos % CHUNK_SIZE as u64) as usize)
}

impl FileContent {
    pub fn logical_size(&self) -> u64 {
        match self {
            FileContent::Empty => 0,
            FileContent::Flat(v) => v.len() as u64,
            FileContent::Chunked { tail_len, chunks } => {
                if chunks.is_empty() {
                    0
                } else {
                    ((chunks.len() - 1) * CHUNK_SIZE) as u64 + *tail_len as u64
                }
            }
        }
    }

    /// Chunk-bookkeeping bytes (accounting class, spec §8).
    pub fn overhead_bytes(&self) -> u64 {
        match self {
            FileContent::Empty | FileContent::Flat(_) => 0,
            FileContent::Chunked { chunks, .. } => {
                (chunks.len() as u64) * CHUNK_OVERHEAD_BYTES
            }
        }
    }

    /// Worst-case charge of a write at `pos` of `len` bytes, WITHOUT
    /// mutating. The subsequent `write_at` is guaranteed to produce a delta
    /// ≤ this (debug-asserted), so a successful pre-flight charge can never
    /// be exceeded (spec §4 ENOSPC discipline).
    pub fn growth_charge(&self, pos: u64, len: usize) -> ContentDelta {
        let old_size = self.logical_size();
        let old_ovh = self.overhead_bytes();
        let new_end = pos.saturating_add(len as u64);
        let content = new_end.saturating_sub(old_size) as i64;
        // Tier discipline (write_at's fix applies here too): a chunked file
        // STAYS chunked — the charge must never assume a flat outcome for an
        // already-chunked content (an in-place low-offset write would
        // otherwise charge a NEGATIVE overhead delta). Only Empty/Flat
        // contents may compute a flat result.
        let is_chunked = matches!(self, FileContent::Chunked { .. });
        let charge_end = if is_chunked { new_end.max(old_size) } else { new_end };
        let chunks_needed = |end: u64| -> u64 {
            if end == 0 {
                0
            } else if end <= FLAT_MAX && !is_chunked {
                // Stays flat — Empty/Flat content ONLY. An ALREADY-CHUNKED
                // content never returns to flat (tier discipline), so its
                // chunk count is always ceil(end/CHUNK_SIZE). P2-008: the
                // missing `!is_chunked` guard here charged a chunked file's
                // sub-FLAT_MAX write with NEGATIVE overhead (found by
                // tests/extraction_fuzz.rs exact_fatal_shape: a 64 KiB
                // chunked file + append crossed the assert; in release the
                // residency accounting silently under-counted by the same
                // delta).
                0
            } else {
                (end as usize + CHUNK_SIZE - 1) as u64 / CHUNK_SIZE as u64
            }
        };
        let new_ovh = chunks_needed(charge_end) * CHUNK_OVERHEAD_BYTES;
        ContentDelta {
            content,
            overhead: new_ovh as i64 - old_ovh as i64,
        }
    }

    /// Read up to `buf.len()` bytes at `pos`. Returns bytes read (0 at/past
    /// EOF — never an error; short reads are legal, spec §4).
    pub fn read_at(&self, pos: u64, buf: &mut [u8]) -> usize {
        let size = self.logical_size();
        if pos >= size || buf.is_empty() {
            return 0;
        }
        let avail = (size - pos) as usize;
        let n = avail.min(buf.len());
        match self {
            FileContent::Empty => unreachable!("size>0 but Empty"),
            FileContent::Flat(v) => {
                buf[..n].copy_from_slice(&v[pos as usize..pos as usize + n]);
                n
            }
            FileContent::Chunked { tail_len, chunks } => {
                let mut done = 0usize;
                let mut off = pos;
                while done < n {
                    let (ci, co) = chunk_index(off);
                    let chunk = &chunks[ci];
                    let chunk_valid = if ci == chunks.len() - 1 {
                        *tail_len as usize
                    } else {
                        CHUNK_SIZE
                    };
                    debug_assert!(co < chunk_valid || chunk_valid == 0);
                    let take = (chunk_valid - co as usize).min(n - done);
                    buf[done..done + take]
                        .copy_from_slice(&chunk[co as usize..co as usize + take]);
                    done += take;
                    off += take as u64;
                }
                n
            }
        }
    }

    /// Write `data` at `pos` (zero-filling any gap — holes are real zeros).
    /// Returns the ACTUAL delta; callers must have pre-flighted
    /// `growth_charge` (debug-asserted here).
    pub fn write_at(&mut self, pos: u64, data: &[u8], charged: ContentDelta) -> ContentDelta {
        let old_size = self.logical_size();
        let old_ovh = self.overhead_bytes() as i64;
        let new_end = pos.saturating_add(data.len() as u64);

        if data.is_empty() {
            return ContentDelta::ZERO;
        }

        // Tier discipline: a file's tier only ever RISES (Empty → Flat →
        // Chunked; B4 demote: truncate(0) → Empty). The write span alone must
        // NOT pick the tier — a low-offset small write onto an ALREADY
        // chunked file stays chunked (found live by the P2-005-005 FD bench
        // bfd7: pwrite 4 KiB at pos < 256 KiB of a 1 MiB file hit the flat
        // branch's unreachable). Only Empty/Flat contents with a fully-flat
        // span take the flat path.
        let is_chunked = matches!(self, FileContent::Chunked { .. });
        if new_end <= FLAT_MAX && !is_chunked {
            // Stays flat.
            if matches!(self, FileContent::Empty) {
                *self = FileContent::Flat(Vec::new());
            }
            if let FileContent::Flat(v) = self {
                if pos > v.len() as u64 {
                    v.resize(pos as usize, 0); // gap zero-fill
                }
                let start = pos as usize;
                let end = start + data.len();
                if end > v.len() {
                    v.resize(end, 0);
                }
                v[start..end].copy_from_slice(data);
            } else {
                unreachable!("flat-range write on chunked content");
            }
        } else {
            // Chunked tier (migrate if flat).
            if let FileContent::Flat(v) = self {
                let full = v.len() / CHUNK_SIZE;
                let rem = v.len() % CHUNK_SIZE;
                let mut chunks: Vec<Box<[u8]>> = Vec::new();
                for i in 0..full {
                    let s = i * CHUNK_SIZE;
                    chunks.push(Box::from(&v[s..s + CHUNK_SIZE]));
                }
                // P2-008 FIX (the extraction write_at trap): the tail chunk
                // MUST be a FULL CHUNK_SIZE allocation — the gap-fill and
                // payload loops index every chunk as [0..CHUNK_SIZE). The
                // old `Box::from(&v[full * CHUNK_SIZE..])` created a SHORT
                // (rem-byte) chunk; the next write landing past byte `rem`
                // of that chunk panicked with a slice range OOB (reproduced
                // by tests/extraction_fuzz.rs: `range end index 65536 out
                // of range for slice of length 2255` — the browser-side
                // `RuntimeError: unreachable` in write_at during
                // create-expo-app's template extraction).
                let tail = if rem > 0 {
                    let mut c = alloc::vec![0u8; CHUNK_SIZE];
                    c[..rem].copy_from_slice(&v[full * CHUNK_SIZE..]);
                    chunks.push(Box::from(c.into_boxed_slice()));
                    rem as u32
                } else if full > 0 {
                    CHUNK_SIZE as u32 // last chunk is full
                } else {
                    0
                };
                *self = FileContent::Chunked { tail_len: tail, chunks };
            }
            if matches!(self, FileContent::Empty) {
                *self = FileContent::Chunked { tail_len: 0, chunks: Vec::new() };
            }
            if let FileContent::Chunked { tail_len, chunks } = self {
                // Zero-fill the gap [old_size, pos) by extending the chunk
                // set with zeroed chunks / a zeroed tail extension.
                let mut covered = old_size;
                while covered < pos {
                    if chunks.is_empty() || *tail_len as usize == CHUNK_SIZE {
                        // new full-zero chunk (it may be partially consumed)
                        chunks.push(Box::from(alloc::vec![0u8; CHUNK_SIZE].into_boxed_slice()));
                        *tail_len = 0;
                    }
                    let room = CHUNK_SIZE - *tail_len as usize;
                    let fill = ((pos - covered) as usize).min(room);
                    // Recycled chunks are dirty: zero the re-exposed tail
                    // bytes (spec §4 — holes are REAL zero bytes).
                    let t0 = *tail_len as usize;
                    chunks.last_mut().unwrap()[t0..t0 + fill].fill(0);
                    *tail_len += fill as u32;
                    covered += fill as u64;
                }
                let _ = covered;
                // Write the payload chunk-by-chunk.
                let mut src = data;
                let mut off = pos;
                while !src.is_empty() {
                    let (ci, co) = chunk_index(off);
                    if ci == chunks.len() {
                        chunks.push(Box::from(
                            alloc::vec![0u8; CHUNK_SIZE].into_boxed_slice(),
                        ));
                        *tail_len = 0;
                    }
                    let take = {
                        let c = &mut chunks[ci];
                        let t = (CHUNK_SIZE - co).min(src.len());
                        c[co..co + t].copy_from_slice(&src[..t]);
                        t
                    };
                    if ci == chunks.len() - 1 {
                        *tail_len = (*tail_len as usize).max(co + take) as u32;
                    }
                    src = &src[take..];
                    off += take as u64;
                }
            }
        }

        let delta = ContentDelta {
            content: (new_end.saturating_sub(old_size)) as i64,
            overhead: self.overhead_bytes() as i64 - old_ovh,
        };
        debug_assert!(delta.content <= charged.content);
        debug_assert!(delta.overhead <= charged.overhead);
        delta
    }

    /// Truncate to `new_len` (grow zero-fills; shrink frees tail chunks).
    /// Offsets of open OFDs are NOT moved (POSIX — the OFD layer's concern).
    pub fn truncate(&mut self, new_len: u64) -> ContentDelta {
        let old_size = self.logical_size();
        let old_ovh = self.overhead_bytes() as i64;
        if new_len == old_size {
            return ContentDelta::ZERO;
        }
        if new_len < old_size {
            if new_len == 0 {
                // Tier demotion (found live by the P2-005-004b B4 bench):
                // truncate-to-0 must return to the Empty variant — a
                // zero-chunk Chunked would trap the next flat-range write
                // (content.rs write_at flat branch) and a zero-len Flat
                // would keep its allocation live forever.
                *self = FileContent::Empty;
                return ContentDelta {
                    content: -(old_size as i64),
                    overhead: -old_ovh,
                };
            }
            match self {
                FileContent::Empty => {}
                FileContent::Flat(v) => v.truncate(new_len as usize),
                FileContent::Chunked { tail_len, chunks } => {
                    {
                        let (ci, co) = chunk_index(new_len);
                        let keep = ci + if co > 0 { 1 } else { 0 };
                        chunks.truncate(keep);
                        *tail_len = if co > 0 { co as u32 } else { CHUNK_SIZE as u32 };
                        if chunks.is_empty() {
                            *tail_len = 0;
                        }
                    }
                }
            }
        } else {
            // Grow: zero-fill.
            match self {
                FileContent::Empty => {
                    if new_len <= FLAT_MAX {
                        *self = FileContent::Flat(alloc::vec![0u8; new_len as usize]);
                    } else {
                        // Deliberately NOT pre-zeroing every chunk here: the
                        // flat→chunked path allocates exact chunks; grow via
                        // zeroed full chunks + an exact tail.
                        // P2-006-008 (kernel panic fix): every chunk is a FULL
                        // CHUNK_SIZE allocation (tail validity rides on
                        // tail_len) — the old rem-sized tail chunk had the
                        // same slice-OOB trap as the migrate path.
                        let full = (new_len / CHUNK_SIZE as u64) as usize;
                        let rem = (new_len % CHUNK_SIZE as u64) as u32;
                        let mut chunks: Vec<Box<[u8]>> = Vec::with_capacity(full + 1);
                        for _ in 0..full {
                            chunks
                                .push(Box::from(alloc::vec![0u8; CHUNK_SIZE].into_boxed_slice()));
                        }
                        let tail = if rem > 0 {
                            // P2-008 FIX: full-size tail chunk (the payload/
                            // gap-fill loops index chunks as [0..CHUNK_SIZE);
                            // a short rem-byte chunk traps the next write —
                            // same class as the migration fix above).
                            chunks.push(Box::from(
                                alloc::vec![0u8; CHUNK_SIZE].into_boxed_slice(),
                            ));
                            rem
                        } else if full > 0 {
                            CHUNK_SIZE as u32
                        } else {
                            0
                        };
                        *self = FileContent::Chunked { tail_len: tail, chunks };
                    }
                }
                FileContent::Flat(v) => {
                    // P2-008: tier discipline — a Flat content grown past
                    // FLAT_MAX by truncate Migrates to Chunked (the write
                    // path's discipline; the old `v.resize(new_len)` kept an
                    // over-size Flat alive, breaking the tier invariant the
                    // fuzzer checks). Sub-FLAT_MAX growth stays flat.
                    if new_len <= FLAT_MAX {
                        v.resize(new_len as usize, 0);
                    } else {
                        v.resize(new_len as usize, 0);
                        let full = v.len() / CHUNK_SIZE;
                        let rem = v.len() % CHUNK_SIZE;
                        let mut chunks: Vec<Box<[u8]>> = Vec::with_capacity(full + 1);
                        for i in 0..full {
                            let s = i * CHUNK_SIZE;
                            chunks.push(Box::from(&v[s..s + CHUNK_SIZE]));
                        }
                        let tail = if rem > 0 {
                            let mut c = alloc::vec![0u8; CHUNK_SIZE];
                            c[..rem].copy_from_slice(&v[full * CHUNK_SIZE..]);
                            chunks.push(Box::from(c.into_boxed_slice()));
                            rem as u32
                        } else {
                            CHUNK_SIZE as u32 // full > 0 (new_len > FLAT_MAX)
                        };
                        *self = FileContent::Chunked { tail_len: tail, chunks };
                    }
                }
                FileContent::Chunked { tail_len, chunks } => {
                    let mut covered = old_size;
                    while covered < new_len {
                        if chunks.is_empty() || *tail_len as usize == CHUNK_SIZE {
                            chunks.push(Box::from(
                                alloc::vec![0u8; CHUNK_SIZE].into_boxed_slice(),
                            ));
                            *tail_len = 0;
                        }
                        let room = CHUNK_SIZE - *tail_len as usize;
                        let fill = ((new_len - covered) as usize).min(room);
                        // Zero the re-exposed bytes (recycled chunks are
                        // dirty — spec §4).
                        let t0 = *tail_len as usize;
                        chunks.last_mut().unwrap()[t0..t0 + fill].fill(0);
                        *tail_len += fill as u32;
                        covered += fill as u64;
                    }
                }
            }
        }
        ContentDelta {
            content: new_len as i64 - old_size as i64,
            overhead: self.overhead_bytes() as i64 - old_ovh,
        }
    }

    /// Append semantics helper: write at current EOF. Atomicity is
    /// structural (single-threaded kernel, spec §9).
    pub fn append_charge(&self, len: usize) -> ContentDelta {
        self.growth_charge(self.logical_size(), len)
    }

    pub fn append(&mut self, data: &[u8], charged: ContentDelta) -> ContentDelta {
        let at = self.logical_size();
        self.write_at(at, data, charged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wr(c: &mut FileContent, pos: u64, data: &[u8]) {
        let ch = c.growth_charge(pos, data.len());
        c.write_at(pos, data, ch);
    }

    /// P2-006-008 regression (kernel panic, found live by the Expo golden
    /// flow): a flat file of non-CHUNK_SIZE-multiple length migrating to
    /// chunks produced a rem-sized TAIL CHUNK; the next write spanning past
    /// it sliced c[co..co+t] with co+t up to CHUNK_SIZE → wasm unreachable.
    /// writeFile of 262,145 B as 8 KiB sync writes hit it exactly
    /// (bisected: 262,144 OK / 262,145 PANIC). Every chunk must be a FULL
    /// CHUNK_SIZE allocation; validity rides on tail_len.
    #[test]
    fn flat_to_chunked_migration_tail_chunk_is_full_sized() {
        // flat 196,608 B (3 full chunks — tail_len = CHUNK_SIZE after migrate)
        // then the killer shape: flat len NOT a chunk multiple.
        let mut c = FileContent::Flat(alloc::vec![5u8; 195_584]); // 2 full + 63,488 rem
        wr(&mut c, 195_584, &alloc::vec![9u8; 66_561]); // crosses into the tail + beyond
        assert!(matches!(c, FileContent::Chunked { .. }));
        assert_eq!(c.logical_size(), 195_584 + 66_561);
        // read back the seam: bytes from the migrated tail and the new write
        let mut buf = [0u8; 16];
        assert_eq!(c.read_at(195_580, &mut buf), 16);
        assert_eq!(&buf[..4], &[5, 5, 5, 5]); // pre-migration data preserved
        assert_eq!(&buf[4..], &[9; 12]); // new payload landed

        // And the exact golden-flow shape: O_TRUNC then 8,128 B sync writes
        // to 262,145 B (writeFile in the browser runtime).
        let mut c2 = FileContent::Empty;
        let total = 262_145usize;
        let mut done = 0usize;
        while done < total {
            let n = core::cmp::min(8_128, total - done);
            wr(&mut c2, done as u64, &alloc::vec![3u8; n]);
            done += n;
        }
        assert_eq!(c2.logical_size(), total as u64);
        let mut probe = [0u8; 8];
        assert_eq!(c2.read_at(total as u64 - 8, &mut probe), 8);
        assert_eq!(&probe, &[3u8; 8]);
    }

    /// P2-006-008 companion: truncate-grow on Empty past FLAT_MAX must also
    /// allocate full-sized tail chunks (same slice-OOB class).
    #[test]
    fn truncate_grow_past_flat_max_has_full_chunks() {
        let mut c = FileContent::Empty;
        let d = c.truncate(300_000);
        assert_eq!(d.content, 300_000);
        assert_eq!(c.logical_size(), 300_000);
        // a write into the final chunk must not panic
        wr(&mut c, 299_992, &[7u8; 8]);
        assert_eq!(c.logical_size(), 300_000);
        let mut probe = [0u8; 4];
        assert_eq!(c.read_at(299_996, &mut probe), 4);
        assert_eq!(&probe, &[7u8; 4]);
    }

    #[test]
    fn tiers_by_size() {
        let mut c = FileContent::Empty;
        assert_eq!(c.logical_size(), 0);
        assert_eq!(c.overhead_bytes(), 0);

        wr(&mut c, 0, &[0u8; 64]);
        assert!(matches!(c, FileContent::Flat(_)));
        assert_eq!(c.logical_size(), 64);

        wr(&mut c, 0, &alloc::vec![7u8; 256 * 1024]);
        assert!(matches!(c, FileContent::Flat(_)));
        assert_eq!(c.logical_size(), FLAT_MAX);

        // One byte past the boundary ⇒ migrate to chunks.
        wr(&mut c, FLAT_MAX, &[9]);
        assert!(matches!(c, FileContent::Chunked { .. }));
        assert_eq!(c.logical_size(), FLAT_MAX + 1);
        assert_eq!(c.overhead_bytes(), 5 * CHUNK_OVERHEAD_BYTES);

        // 1 MiB + 4.
        wr(&mut c, 1024 * 1024, &[1, 2, 3, 4]);
        assert_eq!(c.logical_size(), 1024 * 1024 + 4);
        assert_eq!(c.overhead_bytes(), 17 * CHUNK_OVERHEAD_BYTES);
    }

    #[test]
    fn write_past_eof_zero_fills() {
        let mut c = FileContent::Empty;
        wr(&mut c, 10, b"tail");
        assert_eq!(c.logical_size(), 14);
        let mut buf = [0u8; 14];
        assert_eq!(c.read_at(0, &mut buf), 14);
        assert_eq!(&buf[..10], &[0u8; 10]);
        assert_eq!(&buf[10..], b"tail");
    }

    #[test]
    fn chunked_read_spans_chunks_and_eof() {
        let mut c = FileContent::Empty;
        // > FLAT_MAX forces the chunked tier (4 full chunks + 100 B).
        let data: Vec<u8> = (0..(CHUNK_SIZE * 4 + 100))
            .map(|i| (i % 251) as u8)
            .collect();
        assert!(data.len() as u64 > FLAT_MAX);
        wr(&mut c, 0, &data);
        assert!(matches!(c, FileContent::Chunked { .. }));
        assert_eq!(c.logical_size(), data.len() as u64);
        // Full round-trip.
        let mut back = alloc::vec![0u8; data.len()];
        assert_eq!(c.read_at(0, &mut back), data.len());
        assert_eq!(back, data);
        // Spanning read across the chunk boundary.
        let mut mid = [0u8; 8];
        assert_eq!(c.read_at(CHUNK_SIZE as u64 - 3, &mut mid), 8);
        assert_eq!(&mid[..3], &data[CHUNK_SIZE - 3..CHUNK_SIZE]);
        assert_eq!(&mid[3..], &data[CHUNK_SIZE..CHUNK_SIZE + 5]);
        // At/past EOF → 0, not an error.
        assert_eq!(c.read_at(data.len() as u64, &mut mid), 0);
        assert_eq!(c.read_at(data.len() as u64 + 100, &mut mid), 0);
        // Short read at the tail.
        let mut small = [0u8; 1000];
        assert_eq!(c.read_at(data.len() as u64 - 10, &mut small), 10);
    }

    #[test]
    fn truncate_grow_zero_fill_and_shrink_refund() {
        let mut c = FileContent::Empty;
        wr(&mut c, 0, &alloc::vec![5u8; 300 * 1024]);
        let d = c.truncate(100 * 1024);
        assert_eq!(d.content, -(200 * 1024) as i64);
        assert_eq!(c.logical_size(), 100 * 1024);
        let mut buf = [0u8; 16];
        c.read_at(100 * 1024 - 16, &mut buf);
        assert_eq!(buf, [5u8; 16]);

        let d2 = c.truncate(300 * 1024); // grow: zero-fill
        assert_eq!(d2.content, (200 * 1024) as i64);
        let mut buf2 = [1u8; 16];
        c.read_at(100 * 1024, &mut buf2);
        assert_eq!(buf2, [0u8; 16]); // the gap is zeros

        // Truncate to exact chunk boundary then zero: tier DEMOTION to
        // Empty (P2-005-004b B4 fix — a zero-chunk Chunked would trap the
        // next flat-range write and keep Vec storage live).
        let d3 = c.truncate(0);
        assert_eq!(d3.content, -(300 * 1024) as i64);
        assert_eq!(c.logical_size(), 0);
        assert!(matches!(c, FileContent::Empty), "truncate(0) demotes to Empty");
    }

    #[test]
    fn charge_never_underestimates() {
        let mut c = FileContent::Empty;
        // Gap write: charge covers gap + payload.
        let ch = c.growth_charge(FLAT_MAX + 10, 5);
        c.write_at(FLAT_MAX + 10, &[1, 2, 3, 4, 5], ch);
        assert!(ch.content >= (FLAT_MAX + 15) as i64);
        // Sequential appends: charge == actual.
        let ch2 = c.append_charge(10);
        let d2 = c.append(&alloc::vec![0u8; 10], ch2);
        assert_eq!(ch2, d2);
    }
}
