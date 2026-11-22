//! P2-008: extraction-pattern fuzz — the browser kernel trapped with
//! `RuntimeError: unreachable` inside `FileContent::write_at` during
//! create-expo-app's template extraction (fatal request: async WRITE
//! txLen=65536, sequential offset, fd 4; WORKLOG "known open" — the 240 s
//! install-burst write_at trap). Byte-exact write probes did NOT reproduce
//! it, so this fuzzer mimics the REAL op mix: interleaved multi-file
//! sequential writes at tar/cacache chunk sizes, opens with O_TRUNC,
//! mid-file truncates, appends, and rewrite-in-place.
//!
//! Every op checks the structural invariants; any Rust panic (slice OOB,
//! unwraps, the literal `unreachable!`) surfaces WITH its line here instead
//! of killing the browser kernel.

use kernel_vfs::content::{ContentDelta, FileContent};

/// xorshift64* — deterministic, seedable.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The observed write-size distribution: tar/cacache use 64 KiB; the bridge
/// sync path 8 KiB; small package.json / index writes are tiny.
fn pick_len(rng: &mut Rng) -> usize {
    match rng.below(10) {
        0..=3 => 65536,             // the fatal shape
        4 => 65537,                 // one past
        5 => 65535,                 // one under
        6 => 131072,                // bridge WRITE_CHUNK
        7 => 8128,                  // sync TX cap
        8 => 1 + rng.below(4096) as usize, // small file
        _ => 1 + rng.below(262144) as usize, // arbitrary
    }
}

fn verify(fc: &FileContent) {
    let size = fc.logical_size();
    match fc {
        FileContent::Empty => assert_eq!(size, 0, "Empty with size"),
        FileContent::Flat(_) => assert!(size <= 256 * 1024, "Flat above FLAT_MAX"),
        FileContent::Chunked { tail_len, chunks } => {
            assert!(*tail_len as u64 <= 65536, "tail_len {tail_len} > CHUNK_SIZE");
            if chunks.is_empty() {
                assert_eq!(*tail_len, 0, "empty chunk set with nonzero tail");
                assert_eq!(size, 0);
            } else {
                let expect = if chunks.len() == 1 {
                    *tail_len as u64
                } else {
                    (chunks.len() as u64 - 1) * 65536 + *tail_len as u64
                };
                assert_eq!(
                    size, expect,
                    "logical_size {size} != chunk-derived {expect} ({} chunks, tail {tail_len})",
                    chunks.len()
                );
                if *tail_len as usize == 65536 {
                    // a full last chunk is legal; chunks.len() >= 1
                }
            }
        }
    }
    // Full read-back must not panic (catches chunk coverage holes).
    let mut buf = vec![0u8; size as usize];
    let _ = fc.read_at(0, &mut buf);
}

#[test]
fn fuzz_filecontent_extraction_mix() {
    // Sweep seeds until a panic; each seed is a short op sequence.
    for seed in 1..=20_000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let mut fc = FileContent::default();
        let ops = 4 + rng.below(24);
        for _ in 0..ops {
            match rng.below(10) {
                0..=5 => {
                    // sequential-style append (the bridge's -1 offset ⇒ EOF)
                    let len = pick_len(&mut rng);
                    let charged: ContentDelta = fc.append_charge(len);
                    fc.append(&vec![0xA5u8; len], charged);
                }
                6 => {
                    // positional write at a random offset (rewrite-in-place
                    // AND hole creation — utimes/pacote index rewrites)
                    let len = pick_len(&mut rng);
                    let pos = rng.below(700_000);
                    let charged = fc.growth_charge(pos, len);
                    fc.write_at(pos, &vec![0x3Cu8; len], charged);
                }
                7 | 8 => {
                    // truncate (shrink + grow, chunk-aligned + off-by-one)
                    let nl = match rng.below(4) {
                        0 => 0,
                        1 => rng.below(700_000),
                        2 => 65536 * rng.below(11),
                        _ => 65536 * rng.below(11) + 1,
                    };
                    fc.truncate(nl);
                }
                _ => {}
            }
            verify(&fc);
        }
    }
}

#[test]
fn fuzz_filecontent_exact_fatal_shape() {
    // The exact browser sequence: N sequential 64 KiB appends (tar extracting
    // a large file), interleaved with a SECOND file's small writes, then a
    // truncate of the first to a chunk boundary, then more 64 KiB appends.
    for interleave in [0usize, 1, 2, 4] {
        for trunc_to in [0u64, 65536, 131072, 196608, 262144, 327680] {
            let mut a = FileContent::default();
            let mut b = FileContent::default();
            for i in 0..12 {
                let len = 65536;
                let ch = a.append_charge(len);
                a.append(&vec![1u8; len], ch);
                for _ in 0..interleave {
                    let s = 1 + (i * 997) % 8000;
                    let chb = b.append_charge(s);
                    b.append(&vec![2u8; s], chb);
                }
                if i == 8 {
                    a.truncate(trunc_to);
                }
                eprintln!("case interleave={interleave} trunc_to={trunc_to} i={i} a.size={}", a.logical_size());
                verify(&a);
                verify(&b);
            }
        }
    }
}

#[test]
fn fuzz_filecontent_flat_boundary_migration() {
    // Cross the 256 KiB FLAT_MAX boundary from every offset with every size.
    for start in [0usize, 1, 65535, 65536, 65537, 131072, 196608, 262143, 262144] {
        for grow in [1usize, 2, 65536, 65537, 131072, 262144] {
            let mut fc = FileContent::default();
            let ch = fc.growth_charge(0, start);
            fc.write_at(0, &vec![9u8; start], ch);
            verify(&fc);
            let ch = fc.growth_charge(start as u64, grow);
            fc.write_at(start as u64, &vec![7u8; grow], ch);
            verify(&fc);
            // then one more 64 KiB sequential append (the fatal shape)
            let ch = fc.append_charge(65536);
            fc.append(&vec![5u8; 65536], ch);
            verify(&fc);
        }
    }
}
