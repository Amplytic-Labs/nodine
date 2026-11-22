//! # kernel-vfs-bench — the P2-005-004b B1–B10 microbench battery (B §10.2)
//!
//! One harness, two layers (frozen spec §12): **Layer N** (this crate's host
//! bin; std-backed counting shim) and **Layer W** (this crate's cdylib built
//! for wasm32 with the exact production profile; kalloc counting shim).
//! **All gates evaluate on Layer W.** B7 (fixture histogram) is host-side by
//! nature — it gates design constants, not runtime code (poc/p2-005).
//!
//! Timing: every gate is a batch total (amortizes clock overhead to zero)
//! except B5/B6-resume per-op numbers, which use the per-op clock pair and
//! report its ~0.1–0.4 µs overhead. 5 repetitions per measurement; gates
//! evaluate on the MEDIAN rep unless a bench says otherwise. Fixed-seed
//! splitmix64 PRNG (SEED below) — every workload is reproducible.
//!
//! Output: one JSON object per bench: {"id","layer","reps":[...],"gates":[...]}
//! — `gates[i].pass` false anywhere ⇒ the caller (main / run-bench.cjs)
//! exits non-zero.

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;



use kernel_vfs::{OpenModes, Vfs, VfsAccounting};

pub mod backend;
#[cfg(target_arch = "wasm32")]
pub mod wasm_exports;

/// Frozen workload seed (documented; every generator derives from it).
pub const SEED: u64 = 0x0000_2005_004B_00B1;
pub const REPS: usize = 5;
const NOW: i64 = 1_760_000_000_000_000_000; // fixed timestamp (ns)
const ROOT: u32 = 0;

// ---------------------------------------------------------------------------
// harness helpers
// ---------------------------------------------------------------------------

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Percentile of a NON-EMPTY sorted slice (p in 0..=100).
fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    // floor(x + 0.5) without libm (no_std): round-half-up.
    let x = (p / 100.0) * (sorted.len() - 1) as f64;
    let idx = (x + 0.5) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    pct(&s, 50.0)
}

struct Gate {
    expr: &'static str,
    value: f64,
    pass: bool,
    note: &'static str,
}

fn json_num(v: f64) -> String {
    // v == trunc(v) without libm: a float with no fractional part equals
    // its integer cast round-trip.
    if v >= -9.0e15 && v <= 9.0e15 && (v as i64) as f64 == v {
        format!("{}", v as i64)
    } else {
        format!("{:.3}", v)
    }
}

fn emit(id: &str, layer: &str, reps: &[String], gates: &[Gate], extra: &str) -> String {
    let mut s = format!("{{\"id\":\"{}\",\"layer\":\"{}\",\"reps\":[", id, layer);
    for (i, r) in reps.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(r);
    }
    s.push_str("],\"gates\":[");
    for (i, g) in gates.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"expr\":\"{}\",\"value\":{},\"pass\":{},\"note\":\"{}\"}}",
            g.expr,
            json_num(g.value),
            g.pass,
            g.note
        ));
    }
    s.push_str("]");
    if !extra.is_empty() {
        s.push(',');
        s.push_str(extra);
    }
    s.push('}');
    s
}

fn create_file(v: &mut Vfs, path: &str) -> bool {
    match v.open(
        ROOT,
        path.as_bytes(),
        OpenModes::WRONLY | OpenModes::CREAT | OpenModes::EXCL,
        0o644,
        0o022,
        NOW,
    ) {
        Ok(idx) => {
            v.close(idx).is_ok()
        }
        Err(_) => false,
    }
}

/// The accounted-bytes total for the heap-efficiency gates (spec §8 slack +
/// residency classes — everything the VFS tracks).
fn accounted(a: &VfsAccounting) -> u64 {
    a.inode_bytes + a.dirent_bytes + a.dentry_index_bytes + a.name_bytes + a.content_bytes + a.chunk_overhead_bytes
}

// ---------------------------------------------------------------------------
// B1 — 10k / 100k file create (npm shape)
// ---------------------------------------------------------------------------

/// npm-shaped DIRENT name mix (SLASH-FREE — a dirent name can never contain
/// '/'; the scoped-package shape models "@ns{K}" as a subdirectory created
/// by the skeleton): short literals, scoped-package files, sha1-hex (40 B),
/// long bundle blobs. `i` determines the shape deterministically.
///
/// Buffer discipline: writes into `out` WITHOUT reallocation (clear + push
/// reuses capacity) — the harness must contribute ZERO allocations to the
/// measured allocs/create number.
fn b1_name(i: u64, out: &mut String) {
    out.clear();
    match i % 10 {
        0..=5 => push_decimal_prefixed(out, b'f', i, 5),
        6..=8 => {
            out.push_str("pkg-");
            push_decimal(out, i);
        }
        _ => {
            // 40-char hex (sha1-shaped) — the boxed-name minority, matching
            // B's 100k projection ("long-name boxes ~0.5 MB" = ~10%).
            let mut r = Rng::new(SEED ^ i);
            for _ in 0..40 {
                out.push(core::char::from_digit(r.below(16) as u32, 16).unwrap());
            }
        }
    }
}

/// Build the full path of file `i` into `out` (no reallocation).
fn b1_path_into(i: u64, dir: u64, dirs: u64, name: &str, out: &mut String) {
    out.clear();
    out.push('/');
    out.push('d');
    push_decimal(out, dir);
    out.push('/');
    out.push_str(name);
    let _ = dirs;
}

fn push_decimal(out: &mut String, mut v: u64) {
    let mut tmp = [0u8; 20];
    let mut n = 0;
    loop {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for j in (0..n).rev() {
        out.push(tmp[j] as char);
    }
}

fn push_decimal_prefixed(out: &mut String, prefix: u8, v: u64, width: usize) {
    out.push(prefix as char);
    let mut tmp = [b'0'; 20];
    let mut n = 0;
    let mut x = v;
    loop {
        tmp[n] = b'0' + (x % 10) as u8;
        n += 1;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    while n < width {
        tmp[n] = b'0';
        n += 1;
    }
    for j in (0..n).rev() {
        out.push(tmp[j] as char);
    }
}

fn b1_run(v: &mut Vfs, files: u64, dirs: u64) -> (f64, f64, f64, f64, f64) {
    // Pre-create the directory skeleton: D package dirs (node_modules
    // shape: each package dir holds ~20 files; 10k files -> 500 dirs).
    for i in 0..dirs {
        let d = format!("/d{}", i);
        let _ = v.mkdir(ROOT, d.as_bytes(), 0o755, 0, NOW);
    }
    // Reusable buffers, pre-sized: the measured loop must contribute ZERO
    // harness allocations to allocs/create.
    let mut name = String::with_capacity(255);
    let mut path = String::with_capacity(600);
    let a0 = backend::alloc_stats();
    let t0 = backend::now_ns();
    for i in 0..files {
        b1_name(i, &mut name);
        b1_path_into(i, i % dirs, dirs, &name, &mut path);
        assert!(create_file(v, &path), "B1 create failed at i={}", i);
    }
    let dt = backend::now_ns() - t0;
    let a1 = backend::alloc_stats();
    let creates_per_s = files as f64 / (dt / 1e9);
    let allocs_per_create = (a1.alloc_calls - a0.alloc_calls) as f64 / files as f64;
    let live = if a1.block_live > 0 { a1.block_live } else { a1.live_requested };
    let acc = accounted(v.accounting());
    let ratio = live as f64 / acc as f64;
    (creates_per_s, allocs_per_create, ratio, live as f64, acc as f64)
}

pub fn b1() -> String {
    let mut reps = Vec::new();
    let mut med_10 = [0.0f64; 3];
    let mut med_100 = [0.0f64; 3];
    for &(files, dirs, slot) in &[(10_000u64, 500u64, 0usize), (100_000, 5_000, 3)] {
        let mut runs: Vec<[f64; 5]> = Vec::new();
        for _ in 0..REPS {
            let mut v = Vfs::new(NOW);
            let r = b1_run(&mut v, files, dirs);
            core::mem::drop(v);
            runs.push([r.0, r.1, r.2, r.3, r.4]);
        }
        let cps: Vec<f64> = runs.iter().map(|r| r[0]).collect();
        let apc: Vec<f64> = runs.iter().map(|r| r[1]).collect();
        let rat: Vec<f64> = runs.iter().map(|r| r[2]).collect();
        let lv: Vec<f64> = runs.iter().map(|r| r[3]).collect();
        let ac: Vec<f64> = runs.iter().map(|r| r[4]).collect();
        let m = [median(&cps), median(&apc), median(&rat)];
        if slot == 0 {
            med_10 = m;
        } else {
            med_100 = m;
        }
        reps.push(format!(
            "{{\"files\":{},\"creates_s\":{},\"allocs_create\":{},\"heap_live\":{},\"accounted\":{},\"heap_accounted\":{}}}",
            files,
            json_num(m[0]),
            json_num(m[1]),
            json_num(median(&lv)),
            json_num(median(&ac)),
            json_num(m[2])
        ));
    }
    let layer = layer_name();
    let live_note = if cfg!(target_arch = "wasm32") {
        "kalloc block-live"
    } else {
        "requested-bytes live (Layer N label; gate is Layer W)"
    };
    let gates = [
        Gate { expr: "B1 10k creates/s >= 20000", value: med_10[0], pass: med_10[0] >= 20000.0, note: "median of 5" },
        Gate { expr: "B1 100k creates/s >= 20000", value: med_100[0], pass: med_100[0] >= 20000.0, note: "median of 5" },
        Gate { expr: "B1 allocs/create <= 3", value: med_100[1], pass: med_100[1] <= 3.0, note: "100k run" },
        Gate { expr: "B1 heap_live/accounted <= 1.15", value: med_100[2], pass: med_100[2] <= 1.15, note: live_note },
    ];
    emit("B1", layer, &reps, &gates, "")
}

// ---------------------------------------------------------------------------
// B2 — readdir 10k-entry dir (scan + cookie resume)
// ---------------------------------------------------------------------------

pub fn b2() -> String {
    let mut reps = Vec::new();
    let mut scan_med = 0.0f64;
    let mut resume_p95 = 0.0f64;
    {
        let mut v = Vfs::new(NOW);
        let _ = v.mkdir(ROOT, b"big", 0o755, 0, NOW);
        let mut name = String::new();
        for i in 0..10_000u64 {
            b1_name(i, &mut name);
            let path = format!("/big/{}", name);
            if !create_file(&mut v, &path) {
                return emit("B2", layer_name(), &["\"bootstrap-failed\"".into()], &[Gate { expr: "bootstrap", value: 0.0, pass: false, note: "create failed" }], "");
            }
        }
        // Warmup scan.
        let _ = scan_batches(&v, 1024);
        let mut scans: Vec<f64> = Vec::new();
        for _ in 0..REPS {
            let t0 = backend::now_ns();
            let n = scan_batches(&v, 1024);
            let dt = backend::now_ns() - t0;
            assert_eq!(n, 10_000, "B2 scan count");
            scans.push(dt / 1e6); // ms
        }
        scan_med = median(&scans);
        // Cookie resume: 200 random resumes, per-op timing.
        let mut rng = Rng::new(SEED ^ 0xB2);
        let mut seqs: Vec<u64> = Vec::new();
        {
            let mut cookie = 0u64;
            loop {
                let (next, emitted) = v
                    .readdir_with(ROOT, b"/big", cookie, 512, 1 << 20, |_, _, _, seq| {
                    let _ = seq;
                })
                .unwrap();
                if emitted == 0 {
                    break;
                }
                seqs.push(cookie);
                cookie = next;
                if seqs.len() > 40 {
                    break;
                }
            }
        }
        let mut resumes: Vec<f64> = Vec::new();
        for _ in 0..200u32 {
            let cookie = seqs[rng.below(seqs.len() as u64) as usize];
            let t0 = backend::now_ns();
            let r = v.readdir_with(ROOT, b"/big", cookie, 32, 4096, |_, _, _, _| {});
            let dt = backend::now_ns() - t0;
            if r.is_err() {
                return emit("B2", layer_name(), &["\"resume-error\"".into()], &[Gate { expr: "resume", value: 0.0, pass: false, note: "readdir err" }], "");
            }
            resumes.push(dt / 1e3); // µs
        }
        resumes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        resume_p95 = pct(&resumes, 95.0);
        reps.push(format!(
            "{{\"scan_ms\":{},\"resume_p95_us\":{}}}",
            json_num(scan_med),
            json_num(resume_p95)
        ));
    }
    let gates = [
        Gate { expr: "B2 full scan <= 2 ms", value: scan_med, pass: scan_med <= 2.0, note: "10k entries, batch 1024, median of 5" },
        Gate { expr: "B2 cookie resume <= 50 us", value: resume_p95, pass: resume_p95 <= 50.0, note: "p95 of 200 random resumes (clock overhead ~0.1-0.4us included)" },
    ];
    emit("B2", layer_name(), &reps, &gates, "")
}

fn scan_batches(v: &Vfs, batch: usize) -> usize {
    let mut cookie = 0u64;
    let mut total = 0usize;
    loop {
        let (next, emitted) = v
            .readdir_with(ROOT, b"/big", cookie, batch, 1 << 20, |_, _, _, _| {})
            .unwrap();
        total += emitted;
        if emitted == 0 {
            return total;
        }
        cookie = next;
    }
}

// ---------------------------------------------------------------------------
// B3 — resolution storm (internalModuleStat shape) + 64-deep worst case
// ---------------------------------------------------------------------------

pub fn b3() -> String {
    let mut v = Vfs::new(NOW);
    // 8-deep tree: /r0/r1/.../r7/<files>
    let mut prefix = String::new();
    for d in 0..8u32 {
        prefix.push_str(&format!("/r{}", d));
        let _ = v.mkdir(ROOT, prefix.as_bytes(), 0o755, 0, NOW);
    }
    let mut hot: Vec<String> = Vec::new();
    for i in 0..20u64 {
        let p = format!("{}/hot{}.js", prefix, i);
        if !create_file(&mut v, &p) {
            return emit("B3", layer_name(), &["\"bootstrap-failed\"".into()], &[Gate { expr: "bootstrap", value: 0.0, pass: false, note: "create failed" }], "");
        }
        hot.push(p);
    }
    let mut spread: Vec<String> = Vec::new();
    for i in 0..500u64 {
        let p = format!("{}/mod{}@{}.js", prefix, i, i * 7);
        if !create_file(&mut v, &p) {
            return emit("B3", layer_name(), &["\"bootstrap-failed\"".into()], &[Gate { expr: "bootstrap", value: 0.0, pass: false, note: "create failed" }], "");
        }
        spread.push(p);
    }
    // 64-deep worst case.
    let mut deep = String::new();
    for d in 0..64u32 {
        deep.push_str(&format!("/x{}", d));
        let _ = v.mkdir(ROOT, deep.as_bytes(), 0o755, 0, NOW);
    }
    let deep_file = format!("{}/deep.js", deep);
    if !create_file(&mut v, &deep_file) {
        return emit("B3", layer_name(), &["\"bootstrap-failed\"".into()], &[Gate { expr: "bootstrap", value: 0.0, pass: false, note: "deep create failed" }], "");
    }

    // The 3,489-op mix (npm install internalModuleStat shape): 60% hot-20
    // repeats, 40% spread across 500.
    let mut ops: Vec<&String> = Vec::with_capacity(3489);
    for i in 0..3489usize {
        if i % 10 < 6 {
            ops.push(&hot[i % 20]);
        } else {
            ops.push(&spread[i % 500]);
        }
    }

    // Warmup + alloc-invariant check.
    for p in ops.iter().take(500) {
        let _ = v.stat(ROOT, p.as_bytes());
    }
    let mut totals: Vec<f64> = Vec::new();
    let mut alloc_drift: u64 = 0;
    for _ in 0..REPS {
        let a0 = backend::alloc_stats();
        let t0 = backend::now_ns();
        let mut ok = 0u32;
        for p in ops.iter() {
            if v.stat(ROOT, p.as_bytes()).is_ok() {
                ok += 1;
            }
        }
        let dt = backend::now_ns() - t0;
        let a1 = backend::alloc_stats();
        assert_eq!(ok, 3489, "B3: every stat must resolve");
        alloc_drift = alloc_drift.max(a1.alloc_calls - a0.alloc_calls);
        totals.push(dt / 1e6); // ms
    }
    let med = median(&totals);

    // 64-deep per-op (batch-timed over 1000 ops).
    let mut deep_us: Vec<f64> = Vec::new();
    for _ in 0..REPS {
        let t0 = backend::now_ns();
        for _ in 0..1000u32 {
            let _ = v.stat(ROOT, deep_file.as_bytes());
        }
        let dt = backend::now_ns() - t0;
        deep_us.push(dt / 1000.0 / 1e3); // µs/op
    }
    let deep_med = median(&deep_us);

    let reps = vec![format!(
        "{{\"storm_ms\":{},\"per_op_us\":{},\"deep_us_op\":{},\"alloc_drift\":{}}}",
        json_num(med),
        json_num(med * 1000.0 / 3489.0),
        json_num(deep_med),
        alloc_drift
    )];
    let gates = [
        Gate { expr: "B3 3489-op storm <= 10 ms", value: med, pass: med <= 10.0, note: "median of 5, ~3us/op target" },
        Gate { expr: "B3 64-deep <= 25 us/op", value: deep_med, pass: deep_med <= 25.0, note: "batch mean, median of 5" },
        Gate { expr: "B-alloc invariant: 0 allocations on read-only storm", value: alloc_drift as f64, pass: alloc_drift == 0, note: "B 7.4 read-only-op invariant" },
    ];
    emit("B3", layer_name(), &reps, &gates, "")
}

// ---------------------------------------------------------------------------
// B4 — 16 MiB sequential write then read (256 KiB bridge-shaped chunks)
// ---------------------------------------------------------------------------

pub fn b4() -> String {
    const TOTAL: usize = 16 << 20;
    const CHUNK: usize = 256 << 10;
    let mut reps = Vec::new();
    let mut w_med = 0.0;
    let mut r_med = 0.0;
    let mut trans_med = 0.0f64;
    {
        let mut v = Vfs::new(NOW);
        let idx = v
            .open(ROOT, b"/big.bin", OpenModes::WRONLY | OpenModes::CREAT, 0o644, 0, NOW)
            .unwrap();
        // Pre-allocated chunk buffer (bench infrastructure — excluded from
        // the transient-heap baseline).
        let mut buf: Vec<u8> = Vec::with_capacity(CHUNK);
        buf.resize(CHUNK, 0);
        for (c, b) in buf.chunks_mut(16).enumerate() {
            for (j, x) in b.iter_mut().enumerate() {
                *x = ((c * 16 + j) % 251) as u8;
            }
        }
        let mut writes: Vec<f64> = Vec::new();
        let mut reads: Vec<f64> = Vec::new();
        let mut transients: Vec<f64> = Vec::new();
        let mut cks: Vec<u64> = Vec::new();
        for _ in 0..REPS {
            // truncate to 0 and rewrite (same OFD).
            v.ftruncate_idx(idx, 0, NOW).unwrap();
            let base = backend::alloc_stats();
            let t0 = backend::now_ns();
            for off in 0..(TOTAL / CHUNK) {
                v.write_at(idx, (off * CHUNK) as u64, &buf, NOW).unwrap();
            }
            let dtw = backend::now_ns() - t0;
            let after = backend::alloc_stats();
            let live_after = if after.block_live > 0 { after.block_live } else { after.live_requested };
            let base_live = if base.block_live > 0 { base.block_live } else { base.live_requested };
            // transient = growth during the write (content + migration copies).
            let hw = if after.block_high_water > 0 { after.block_high_water } else { after.high_water_requested };
            let base_hw = if base.block_high_water > 0 { base.block_high_water } else { base.high_water_requested };
            let transient = (live_after - base_live).max(hw - base_hw) as f64;
            writes.push(dtw / 1e6);
            transients.push(transient / (1024.0 * 1024.0));
            // read back: TIMED pure kernel reads, then an UNTIMED checksum
            // pass (the 16M-op wrapping hash is harness compute, not kernel
            // time — it was polluting the <=15 ms gate).
            let mut rbuf: Vec<u8> = Vec::with_capacity(CHUNK);
            rbuf.resize(CHUNK, 0);
            let t1 = backend::now_ns();
            for off in 0..(TOTAL / CHUNK) {
                let n = v.read_at(idx, (off * CHUNK) as u64, &mut rbuf, NOW).unwrap();
                assert_eq!(n, CHUNK, "B4 short read");
            }
            let dtr = backend::now_ns() - t1;
            reads.push(dtr / 1e6);
            let mut ck: u64 = 0;
            for off in 0..(TOTAL / CHUNK) {
                let _ = v.read_at(idx, (off * CHUNK) as u64, &mut rbuf, NOW).unwrap();
                for b in rbuf.iter() {
                    ck = ck.wrapping_mul(31).wrapping_add(*b as u64);
                }
            }
            cks.push(ck);
        }
        assert!(
            cks.windows(2).all(|w| w[0] == w[1]),
            "B4: content checksum must be identical across reps"
        );
        w_med = median(&writes);
        r_med = median(&reads);
        trans_med = median(&transients);
        reps.push(format!(
            "{{\"write_ms\":{},\"read_ms\":{},\"transient_mib\":{},\"checksum\":{}}}",
            json_num(w_med),
            json_num(r_med),
            json_num(trans_med),
            cks[0]
        ));
        core::mem::drop(v);
    }
    let gates = [
        Gate { expr: "B4 write <= 20 ms", value: w_med, pass: w_med <= 20.0, note: "16 MiB in 256 KiB chunks, median of 5" },
        Gate { expr: "B4 read <= 15 ms", value: r_med, pass: r_med <= 15.0, note: "median of 5" },
        Gate { expr: "B4 transient heap <= 17 MiB", value: trans_med, pass: trans_med <= 17.0, note: "high-water delta above pre-write live (chunk buffer pre-allocated)" },
    ];
    emit("B4", layer_name(), &reps, &gates, "")
}

// ---------------------------------------------------------------------------
// B5 — insert/remove p99 at widths 1k/10k/30k/100k (random + reverse arrival)
// ---------------------------------------------------------------------------

fn b5_measure(v: &mut Vfs, dir: &str, existing: u64, extra: u64, reverse: bool, seed: u64) -> (f64, f64, f64) {
    // dir already holds `existing` entries; time `extra` more inserts
    // (per-op clock) and return (p50, p95, p99) in µs.
    //
    // Sort-position discipline (fixed in P2-005-004b after B9 exposed it):
    // the RANDOM key must lead (rng digits first, index second) so arrival
    // positions are uniformly spread through the existing set — a
    // sequential leading index makes every "random" insert an append (no
    // by_name memmove at all). TRUE reverse arrival = strictly descending
    // keys: every insert lands at position 0 (worst-case memmove).
    let mut names: Vec<String> = Vec::with_capacity(extra as usize);
    let mut r = Rng::new(seed);
    for i in 0..extra {
        names.push(format!("m-{:08}-{:06}", r.below(1 << 20), existing + i));
    }
    if reverse {
        // Descending keys: next insert is smaller than everything present.
        names.sort();
        names.reverse();
    }
    let mut times: Vec<f64> = Vec::with_capacity(extra as usize);
    for n in names.iter() {
        let path = format!("{}/{}", dir, n);
        let t0 = backend::now_ns();
        let ok = create_file(v, &path);
        let dt = backend::now_ns() - t0;
        assert!(ok, "B5 insert failed: {}", n);
        times.push(dt / 1e3);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    (pct(&times, 50.0), pct(&times, 95.0), pct(&times, 99.0))
}

pub fn b5() -> String {
    let mut reps = Vec::new();
    let mut p99_10k = 0.0f64;
    let mut p99_30k = 0.0f64;
    for &(width, extra) in &[(1_000u64, 500u64), (10_000, 1_000), (30_000, 1_000), (100_000, 1_000)] {
        let mut v = Vfs::new(NOW);
        let _ = v.mkdir(ROOT, format!("w{}", width).as_bytes(), 0o755, 0, NOW);
        let dir = format!("/w{}", width);
        // Fill to width (untimed) — random arrival.
        let mut r = Rng::new(SEED ^ width);
        let mut fill: Vec<String> = Vec::with_capacity(width as usize);
        for i in 0..width {
            fill.push(format!("m-{:06}-{:08}", i, r.below(1 << 20)));
        }
        fill.sort();
        fill.dedup();
        for n in fill.iter() {
            let path = format!("{}/{}", dir, n);
            assert!(create_file(&mut v, &path), "B5 fill failed");
        }
        // Random-arrival inserts (single measured pass per width; the fills
        // at 100k are the expensive part — 5 reps of the full build would
        // measure tree construction, not insert p99).
        let (p50, p95, p99) = b5_measure(&mut v, &dir, width, extra, false, SEED ^ 0xB5 ^ width);
        let (_r50, _r95, r99) = b5_measure(&mut v, &dir, width + extra, extra, true, SEED ^ 0xB5D ^ width);
        if width == 10_000 {
            p99_10k = p99;
        }
        if width == 30_000 {
            p99_30k = p99;
        }
        reps.push(format!(
            "{{\"width\":{},\"rand_p50_us\":{},\"rand_p95_us\":{},\"rand_p99_us\":{},\"rev_p99_us\":{}}}",
            width,
            json_num(p50),
            json_num(p95),
            json_num(p99),
            json_num(r99)
        ));
        core::mem::drop(v);
    }
    let gates = [
        Gate { expr: "B5 insert p99 <= 25 us @10k", value: p99_10k, pass: p99_10k <= 25.0, note: "random arrival, per-op clock (~0.1-0.4us overhead included)" },
        Gate { expr: "B5 insert p99 <= 100 us @30k", value: p99_30k, pass: p99_30k <= 100.0, note: "random arrival; >100us => BTreeMap fallback (D2a)" },
    ];
    emit("B5", layer_name(), &reps, &gates, "")
}

// ---------------------------------------------------------------------------
// B6 — readdir during mutation (cookie contract)
// ---------------------------------------------------------------------------
pub fn b6() -> String {
    let mut v = Vfs::new(NOW);
    let _ = v.mkdir(ROOT, b"live", 0o755, 0, NOW);
    let mut names: Vec<String> = Vec::new();
    let mut r = Rng::new(SEED ^ 0xB6);
    for i in 0..5_000u64 {
        let n = format!("e-{:05}-{:06}", i, r.below(1 << 20));
        let path = format!("/live/{}", n);
        assert!(create_file(&mut v, &path), "B6 fill");
        names.push(n);
    }
    let mut seen: Vec<String> = Vec::new();

    // Case 1 — same-generation batches (no mutation between): zero dup/miss.
    let mut dup = 0u64;
    let mut cookie = 0u64;
    loop {
        let (next, emitted) = v
            .readdir_with(ROOT, b"/live", cookie, 512, 1 << 20, |name, kind, ino, seq| {
                let _ = (kind, ino, seq);
                seen.push(String::from_utf8_lossy(name).into_owned());
            })
            .unwrap();
        if emitted == 0 {
            break;
        }
        cookie = next;
    }
    seen.sort();
    seen.dedup();
    let miss = names.iter().filter(|n| !seen.binary_search(n).is_ok()).count() as u64;
    dup = 5000 - seen.len() as u64;
    let same_gen_ok = dup == 0 && miss == 0;

    // Case 2 — mutations between batches: gen-bump resume — never an error,
    // never a trap, pre-existing unmutated entries never lost.
    let mut seen2: Vec<String> = Vec::new();
    let mut cookie = 0u64;
    let mut round = 0u64;
    let mut errors = 0u64;
    loop {
        let (next, emitted) = match v.readdir_with(ROOT, b"/live", cookie, 256, 1 << 20, |name, _, _, _| {
            seen2.push(String::from_utf8_lossy(name).into_owned());
        }) {
            Ok(x) => x,
            Err(_) => {
                errors += 1;
                break;
            }
        };
        if emitted == 0 || round >= 200 {
            break;
        }
        cookie = next;
        round += 1;
        // Mutate between batches: remove one old, add one new. (Round cap:
        // without it the fresh entries keep the cursor emitting forever —
        // the first Layer W run accidentally soaked to the 2^20 inode cap.)
        if (round as usize) < names.len() {
            let victim = format!("/live/{}", names[round as usize]);
            let _ = v.unlink(ROOT, victim.as_bytes(), NOW);
        }
        let fresh = format!("new-{:05}", round);
        let path = format!("/live/{}", fresh);
        let _ = create_file(&mut v, &path);
    }
    seen2.sort();
    seen2.dedup();
    // Unmutated originals = every name EXCEPT the unlinked prefix — none of
    // them may be lost by the interleaved batches (insertions always land
    // after the cursor; deletions behind it are already emitted).
    let removed = (round as usize).min(names.len());
    let lost = names[removed..]
        .iter()
        .filter(|n| !seen2.binary_search(n).is_ok())
        .count() as u64;
    let gen_bump_ok = errors == 0 && lost == 0;
    let rounds = round;

    let reps = vec![format!(
        "{{\"same_gen_dup\":{},\"same_gen_miss\":{},\"gen_bump_errors\":{},\"gen_bump_lost\":{},\"batches\":{}}}",
        dup, miss, errors, lost, rounds
    )];
    let gates = [
        Gate { expr: "B6 same-gen: zero dup", value: dup as f64, pass: dup == 0, note: "5000 entries, batch 512" },
        Gate { expr: "B6 same-gen: zero miss", value: miss as f64, pass: miss == 0, note: "" },
        Gate { expr: "B6 gen-bump: zero errors (documented resume, never error)", value: errors as f64, pass: errors == 0, note: "" },
        Gate { expr: "B6 gen-bump: zero lost unmutated entries", value: lost as f64, pass: gen_bump_ok, note: "" },
    ];
    emit("B6", layer_name(), &reps, &gates, "")
}

// ---------------------------------------------------------------------------
// B8 — fragmentation soak: create 100k -> rm random 50% -> create 50k more
// ---------------------------------------------------------------------------

pub fn b8() -> String {
    let mut reps = Vec::new();
    let mut ratio = 0.0f64;
    for _ in 0..REPS {
        let mut v = Vfs::new(NOW);
        let _ = v.mkdir(ROOT, b"soak", 0o755, 0, NOW);
        let mut all: Vec<String> = Vec::with_capacity(100_000);
        let mut r = Rng::new(SEED ^ 0xB8);
        let mut name = String::new();
        for i in 0..100_000u64 {
            b1_name(i, &mut name);
            let n = format!("{}-{}", name, r.below(1 << 20));
            let path = format!("/soak/{}", n);
            assert!(create_file(&mut v, &path), "B8 create");
            all.push(n);
        }
        // rm random 50% (shuffle by PRNG selection).
        let mut keep: Vec<String> = Vec::with_capacity(50_000);
        for (i, n) in all.iter().enumerate() {
            if r.below(2) == 0 && i % 2 == 0 || r.below(3) == 0 {
                let path = format!("/soak/{}", n);
                assert!(v.unlink(ROOT, path.as_bytes(), NOW).is_ok(), "B8 rm");
            } else {
                keep.push(n.clone());
            }
        }
        // create 50k more.
        for i in 0..50_000u64 {
            b1_name(i + 7_000_000, &mut name);
            let n = format!("{}+{}", name, r.below(1 << 20));
            let path = format!("/soak/{}", n);
            assert!(create_file(&mut v, &path), "B8 re-create");
            keep.push(n);
        }
        let st = backend::alloc_stats();
        let live = if st.block_live > 0 { st.block_live } else { st.live_requested };
        let hw = if st.block_high_water > 0 { st.block_high_water } else { st.high_water_requested };
        let acc = accounted(v.accounting());
        ratio = hw as f64 / live as f64;
        reps.push(format!(
            "{{\"entries\":{},\"live\":{},\"high_water\":{},\"hw_live\":{},\"live_accounted\":{}}}",
            keep.len(),
            live,
            hw,
            json_num(ratio),
            json_num(live as f64 / acc as f64)
        ));
        core::mem::drop(v);
    }
    let med = {
        let rs: Vec<f64> = reps
            .iter()
            .map(|s| {
                // parse hw_live back — simpler: recompute from stored ratio
                let a = s.find("\"hw_live\":").unwrap() + 10;
                let b = s[a..].find(',').map(|x| a + x).unwrap_or(s.len() - 1);
                s[a..b].parse::<f64>().unwrap_or(0.0)
            })
            .collect();
        median(&rs)
    };
    let gates = [
        Gate { expr: "B8 heap_high_water <= 1.25 x heap_live", value: med, pass: med <= 1.25, note: "after create-100k/rm-50%/create-50k, median of 5 (D5b sub-arena rejection gate)" },
    ];
    emit("B8", layer_name(), &reps, &gates, "")
}

// ---------------------------------------------------------------------------
// B9 — hostile names (reverse-sorted, common-prefix clusters, 255 B)
// ---------------------------------------------------------------------------

pub fn b9() -> String {
    let mut reps = Vec::new();
    let mut ratio = 0.0f64;
    {
        let width = 100_000u64;
        let extra = 1_000u64;
        // Baseline: random arrival at width (B5 shape).
        let mut v = Vfs::new(NOW);
        let _ = v.mkdir(ROOT, b"h", 0o755, 0, NOW);
        let mut r = Rng::new(SEED ^ 0xB9);
        let mut fill: Vec<String> = Vec::with_capacity(width as usize);
        for i in 0..width {
            fill.push(format!("m-{:06}-{:08}", i, r.below(1 << 20)));
        }
        fill.sort();
        fill.dedup();
        for n in fill.iter() {
            let path = format!("/h/{}", n);
            assert!(create_file(&mut v, &path), "B9 fill");
        }
        let (p50r, p95r, p99r) = b5_measure(&mut v, "/h", width, extra, false, SEED ^ 0xB9A);
        core::mem::drop(v);

        // Hostile: reverse-sorted arrival + common-prefix clusters + 255 B.
        let mut v = Vfs::new(NOW);
        let _ = v.mkdir(ROOT, b"h2", 0o755, 0, NOW);
        let mut fill: Vec<String> = Vec::with_capacity(width as usize);
        for i in 0..width {
            let kind = i % 3;
            let n = match kind {
                0 => format!("{:08}a", width - i), // reverse-sorted
                1 => format!(
                    "common-prefix-cluster-{:06}.vendor.min.js.map",
                    i
                ), // 30-char shared prefix cluster
                _ => {
                    // 255-byte name (NAME_MAX)
                    let base = format!("h{:08}", i);
                    let pad = 255 - base.len();
                    format!("{}{}", base, "p".repeat(pad))
                }
            };
            fill.push(n);
        }
        for n in fill.iter() {
            let path = format!("/h2/{}", n);
            assert!(create_file(&mut v, &path), "B9 hostile fill");
        }
        // hostile extra inserts: reverse-sorted + prefix-clustered
        let mut names: Vec<String> = Vec::with_capacity(extra as usize);
        for i in 0..extra {
            if i % 2 == 0 {
                names.push(format!("{:08}z", extra - i));
            } else {
                names.push(format!("common-prefix-cluster-{:06}.vendor.min.js.map", width + i));
            }
        }
        let mut times: Vec<f64> = Vec::with_capacity(extra as usize);
        for n in names.iter() {
            let path = format!("/h2/{}", n);
            let t0 = backend::now_ns();
            let ok = create_file(&mut v, &path);
            let dt = backend::now_ns() - t0;
            assert!(ok, "B9 hostile insert failed");
            times.push(dt / 1e3);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        let p50h = pct(&times, 50.0);
        let p99h = pct(&times, 99.0);
        ratio = p50h / p50r.max(0.001);
        reps.push(format!(
            "{{\"rand_p50_us\":{},\"rand_p99_us\":{},\"hostile_p50_us\":{},\"hostile_p99_us\":{},\"hostile_rand_p50\":{}}}",
            json_num(p50r),
            json_num(p99r),
            json_num(p50h),
            json_num(p99h),
            json_num(ratio)
        ));
    }
    let gates = [
        Gate { expr: "B9 hostile p50 <= 2x random p50", value: ratio, pass: ratio <= 2.0, note: "no O(n^2) blowup; binary search unaffected by hostile ordering" },
    ];
    emit("B9", layer_name(), &reps, &gates, "")
}

// ---------------------------------------------------------------------------
// B10 — stat encode: 1M x 18-field pack
// ---------------------------------------------------------------------------

pub fn b10() -> String {
    let mut v = Vfs::new(NOW);
    let idx = v
        .open(ROOT, b"/s", OpenModes::WRONLY | OpenModes::CREAT, 0o644, 0, NOW)
        .unwrap();
    v.write_at(idx, 0, &[7u8; 4096], NOW).unwrap();
    let mut reps = Vec::new();
    let mut ns_per = 0.0f64;
    for _ in 0..REPS {
        // batch-timed: 1M stat_idx + field consumption
        let mut sink: u64 = 0;
        let t0 = backend::now_ns();
        for _ in 0..1_000_000u32 {
            let s = v.stat_idx(idx).unwrap();
            sink = sink.wrapping_add(s.size.wrapping_add(s.ino).wrapping_add(s.mtime_nsec as u64));
        }
        let dt = backend::now_ns() - t0;
        assert_ne!(sink, 0xDEAD_BEEF, "B10 sink must be consumed");
        ns_per = dt / 1_000_000.0;
        reps.push(json_num(ns_per));
    }
    let med = {
        let rs: Vec<f64> = reps.iter().map(|s| s.parse::<f64>().unwrap_or(0.0)).collect();
        median(&rs)
    };
    let gates = [
        Gate { expr: "B10 stat encode <= 150 ns/op", value: med, pass: med <= 150.0, note: "batch mean over 1M ops, median of 5" },
    ];
    emit("B10", layer_name(), &reps, &gates, "")
}

// ---------------------------------------------------------------------------
// driver
// ---------------------------------------------------------------------------

pub fn layer_name() -> &'static str {
    if cfg!(target_arch = "wasm32") {
        "wasm"
    } else {
        "native"
    }
}

/// Host-only debug marker (probe bench 0).
#[cfg(not(target_arch = "wasm32"))]
fn dbg(s: &str) {
    eprintln!("[probe] {}", s);
}
#[cfg(target_arch = "wasm32")]
fn dbg(_s: &str) {}

/// Probe bench (id 0): step-timed sanity path. Not part of the battery.
pub fn b0() -> String {
    dbg("p0 enter");
    let mut v = Vfs::new(NOW);
    dbg("p1 vfs-new");
    let idx = v.open(ROOT, b"/s", OpenModes::WRONLY | OpenModes::CREAT, 0o644, 0, NOW).unwrap();
    dbg("p2 open");
    v.write_at(idx, 0, &[7u8; 4096], NOW).unwrap();
    dbg("p3 write");
    let t0 = backend::now_ns();
    let mut sink = 0u64;
    for _ in 0..1_000_000u32 {
        let s = v.stat_idx(idx).unwrap();
        sink = sink.wrapping_add(s.ino);
    }
    let dt = backend::now_ns() - t0;
    dbg(&format!("p4 1M stat_idx = {:.3} ms (sink {})", dt / 1e6, sink));
    let t5 = backend::now_ns();
    let mut vv: Vec<u8> = Vec::new();
    for i in 0..100_000u32 {
        vv.push(i as u8);
        if vv.len() == vv.capacity() {
            vv.reserve(1);
        }
    }
    let dt5 = backend::now_ns() - t5;
    dbg(&format!("p5 100k vec churn = {:.3} ms", dt5 / 1e6));

    // Per-create allocation census: N creates into a PRE-BUILT dir.
    for &n in &[1000u32, 2000] {
        let mut v2 = Vfs::new(NOW);
        let _ = v2.mkdir(ROOT, b"c", 0o755, 0, NOW);
        // warm the dir to 32 entries so the census sees steady state
        let mut nm = String::with_capacity(64);
        let mut pth = String::with_capacity(80);
        for i in 0..32u64 {
            nm.clear();
            push_decimal_prefixed(&mut nm, b'w', i, 5);
            pth.clear();
            pth.push_str("/c/");
            pth.push_str(&nm);
            assert!(create_file(&mut v2, &pth));
        }
        let a = backend::alloc_stats();
        for i in 0..n as u64 {
            nm.clear();
            push_decimal_prefixed(&mut nm, b'x', i, 5);
            pth.clear();
            pth.push_str("/c/");
            pth.push_str(&nm);
            assert!(create_file(&mut v2, &pth));
        }
        let b = backend::alloc_stats();
        dbg(&format!(
            "p6 census: {} creates -> {} allocs ({:.3}/create)",
            n,
            b.alloc_calls - a.alloc_calls,
            (b.alloc_calls - a.alloc_calls) as f64 / n as f64
        ));
    }
    emit("B0", layer_name(), &[format!("{{\"stat_ms\":{},\"churn_ms\":{}}}", json_num(dt / 1e6), json_num(dt5 / 1e6))], &[], "")
}


// ---------------------------------------------------------------------------
// P2-005-005 FD battery (B-FD-1..12 — FD_SPEC §12; directive §10)
// ---------------------------------------------------------------------------

use kernel_vfs::fd::{FdCaps, FdState, FD_MAX_PER_PROC, OFD_MAX_GLOBAL};

/// FD bench harness: one Vfs + one FdState, N processes. Paths under /p{pid}.
fn fd_boot_procs(n: u32) -> (Vfs, FdState) {
    let mut v = Vfs::new(NOW);
    let mut st = FdState::new();
    for pid in 1..=n {
        let _ = st.process_birth(&mut v, pid, 0o022);
        let d = format!("/p{}", pid);
        let _ = v.mkdir(0, d.as_bytes(), 0o755, 0, NOW);
    }
    (v, st)
}

fn fd_open(st: &mut FdState, v: &mut Vfs, pid: u32, i: u64) -> u32 {
    let path = format!("/p{}/f{}", pid, i);
    st.open(v, pid, path.as_bytes(), OpenModes::RDWR | OpenModes::CREAT, 0o666, NOW)
        .unwrap_or(u32::MAX)
}

// B-FD-1: allocate 10k FDs in one process (fd_max override for the shape).
pub fn bfd1() -> String {
    let mut rates = Vec::new();
    let mut per_fd_ns = Vec::new();
    for _ in 0..REPS {
        let (mut v, mut st) = fd_boot_procs(1);
        st.caps = FdCaps { fd_max: FD_MAX_PER_PROC, ofd_max: OFD_MAX_GLOBAL };
        let t0 = backend::now_ns();
        for i in 0..10_000u64 {
            let fd = fd_open(&mut st, &mut v, 1, i);
            debug_assert!(fd != u32::MAX);
        }
        let dt = backend::now_ns() - t0;
        rates.push(10_000.0 / (dt / 1e9));
        per_fd_ns.push(dt as f64 / 10_000.0);
        core::mem::drop((v, st));
    }
    let layer = layer_name();
    let gates = [
        Gate { expr: "B-FD-1 allocate 10k FDs >= 200k/s", value: median(&rates), pass: median(&rates) >= 200_000.0, note: "median of 5, open(RDWR|CREAT) storm incl. VFS create" },
        Gate { expr: "B-FD-1 per-fd alloc+install <= 5 us", value: median(&per_fd_ns), pass: median(&per_fd_ns) <= 5_000.0, note: "median of 5" },
    ];
    reps_fmt(layer, "B-FD1", &mut [("allocs_s", &rates), ("per_fd_ns", &per_fd_ns)], &gates)
}

// B-FD-2: open/close churn (repeated alloc+close throughput, fd reuse).
pub fn bfd2() -> String {
    let mut rates = Vec::new();
    for _ in 0..REPS {
        let (mut v, mut st) = fd_boot_procs(1);
        for i in 0..64u64 {
            let _ = fd_open(&mut st, &mut v, 1, i); // 64 base files
        }
        let t0 = backend::now_ns();
        const N: u64 = 100_000;
        for i in 0..N {
            let fd = st.open(v_ref(&mut v), 1, b"/p1/f0", OpenModes::RDONLY, 0, NOW).unwrap();
            st.close(v_ref(&mut v), 1, fd).unwrap();
            let _ = i;
        }
        let dt = backend::now_ns() - t0;
        rates.push((2.0 * N as f64) / (dt / 1e9)); // open+close pairs
        core::mem::drop((v, st));
    }
    let layer = layer_name();
    let gates = [
        Gate { expr: "B-FD-2 open+close churn >= 500k ops/s", value: median(&rates), pass: median(&rates) >= 500_000.0, note: "100k open/close pairs, median of 5 (record-only gate family)" },
    ];
    reps_fmt(layer, "B-FD2", &mut [("pairs_s", &rates)], &gates)
}

// B-FD-3: FD lookup latency (warm slab, fstat through the fd layer).
pub fn bfd3() -> String {
    let mut p50s = Vec::new();
    let mut p99s = Vec::new();
    for _ in 0..REPS {
        let (mut v, mut st) = fd_boot_procs(1);
        for i in 0..1024u64 {
            let _ = fd_open(&mut st, &mut v, 1, i);
        }
        let mut per = Vec::new();
        for i in 0..4096u64 {
            let fd = (3 + (i % 1024)) as u32;
            let t0 = backend::now_ns();
            let _ = st.fstat(&v, 1, fd);
            per.push((backend::now_ns() - t0) as f64);
        }
        per.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        p50s.push(pct(&per, 50.0));
        p99s.push(pct(&per, 99.0));
        core::mem::drop((v, st));
    }
    let layer = layer_name();
    let gates = [
        Gate { expr: "B-FD-3 fd lookup (fstat) p50 <= 2 us", value: median(&p50s), pass: median(&p50s) <= 2_000.0, note: "per-op clock (~0.1-0.4us overhead), 1024 warm fds" },
        Gate { expr: "B-FD-3 fd lookup p99 <= 8 us", value: median(&p99s), pass: median(&p99s) <= 8_000.0, note: "" },
    ];
    reps_fmt(layer, "B-FD3", &mut [("p50_ns", &p50s), ("p99_ns", &p99s)], &gates)
}

// B-FD-4: open/close small files end-to-end (the Node writeFile shape).
pub fn bfd4() -> String {
    let mut rates = Vec::new();
    for _ in 0..REPS {
        let (mut v, mut st) = fd_boot_procs(1);
        let t0 = backend::now_ns();
        const N: u64 = 20_000;
        for i in 0..N {
            let path = format!("/p{}", i % 1);
            let fd = st.open(&mut v, 1, format!("/p1/s{}", i).as_bytes(), OpenModes::WRONLY | OpenModes::CREAT | OpenModes::TRUNC, 0o666, NOW).unwrap();
            st.write(&mut v, 1, fd, -1, b"small-file-payload-32-bytes____", NOW).unwrap();
            st.close(&mut v, 1, fd).unwrap();
            let _ = path;
        }
        let dt = backend::now_ns() - t0;
        rates.push(N as f64 / (dt / 1e9));
        core::mem::drop((v, st));
    }
    let layer = layer_name();
    let gates = [
        Gate { expr: "B-FD-4 open+write+close >= 100k files/s", value: median(&rates), pass: median(&rates) >= 100_000.0, note: "32-B payload, median of 5" },
    ];
    reps_fmt(layer, "B-FD4", &mut [("files_s", &rates)], &gates)
}

// B-FD-5/6: sequential read/write throughput through the FD layer (64 KiB ops).
pub fn bfd5() -> String {
    let mut read_mibs = Vec::new();
    let mut write_mibs = Vec::new();
    for _ in 0..REPS {
        let (mut v, mut st) = fd_boot_procs(1);
        let fd = fd_open(&mut st, &mut v, 1, 0);
        let chunk = [7u8; 65536];
        let t0 = backend::now_ns();
        for _ in 0..256 {
            st.write(&mut v, 1, fd, -1, &chunk, NOW).unwrap(); // 16 MiB
        }
        let dt_w = backend::now_ns() - t0;
        let mut buf = [0u8; 65536];
        let t1 = backend::now_ns();
        for _ in 0..256 {
            st.read(&mut v, 1, fd, -1, &mut buf, NOW).unwrap();
        }
        let dt_r = backend::now_ns() - t1;
        write_mibs.push(16.0 / (dt_w / 1e9));
        read_mibs.push(16.0 / (dt_r / 1e9));
        st.close(&mut v, 1, fd).unwrap();
        core::mem::drop((v, st));
    }
    let layer = layer_name();
    let gates = [
        Gate { expr: "B-FD-5 sequential read >= 500 MiB/s", value: median(&read_mibs), pass: median(&read_mibs) >= 500.0, note: "16 MiB in 64 KiB ops, median of 5" },
        Gate { expr: "B-FD-6 sequential write >= 500 MiB/s", value: median(&write_mibs), pass: median(&write_mibs) >= 500.0, note: "" },
    ];
    reps_fmt(layer, "B-FD5", &mut [("read_mib_s", &read_mibs), ("write_mib_s", &write_mibs)], &gates)
}

// B-FD-7/8: positional read/write latency (pwrite/pread, 4 KiB ops).
pub fn bfd7() -> String {
    let mut rp50 = Vec::new();
    let mut wp50 = Vec::new();
    for _ in 0..REPS {
        let (mut v, mut st) = fd_boot_procs(1);
        let fd = fd_open(&mut st, &mut v, 1, 0);
        let zero = [0u8; 1048576];
        st.write(&mut v, 1, fd, 0, &zero, NOW).unwrap(); // 1 MiB file
        let mut rng = Rng::new(SEED ^ 0xBFD);
        let mut rper = Vec::new();
        let mut wper = Vec::new();
        let mut rbuf = [0u8; 4096];
        let mut wbuf = [9u8; 4096];
        for _ in 0..2048 {
            let pos = (rng.below(256)) as i64 * 4096;
            let t0 = backend::now_ns();
            st.read(&mut v, 1, fd, pos, &mut rbuf, NOW).unwrap();
            rper.push((backend::now_ns() - t0) as f64);
            let t1 = backend::now_ns();
            st.write(&mut v, 1, fd, pos, &wbuf, NOW).unwrap();
            wper.push((backend::now_ns() - t1) as f64);
        }
        rper.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        wper.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        rp50.push(pct(&rper, 50.0));
        wp50.push(pct(&wper, 50.0));
        st.close(&mut v, 1, fd).unwrap();
        core::mem::drop((v, st));
    }
    let layer = layer_name();
    let gates = [
        Gate { expr: "B-FD-7 positional read p50 <= 3 us (4 KiB)", value: median(&rp50), pass: median(&rp50) <= 3_000.0, note: "per-op clock, 256 KiB file, median of 5" },
        Gate { expr: "B-FD-8 positional write p50 <= 4 us (4 KiB)", value: median(&wp50), pass: median(&wp50) <= 4_000.0, note: "" },
    ];
    reps_fmt(layer, "B-FD7", &mut [("pread_p50_ns", &rp50), ("pwrite_p50_ns", &wp50)], &gates)
}

// B-FD-9: many descriptors to one inode (1021 independent opens + reads).
pub fn bfd9() -> String {
    let mut ns = Vec::new();
    for _ in 0..REPS {
        let (mut v, mut st) = fd_boot_procs(1);
        // one create+fill+close (fd budget: stdio 3 + 1021 opens = 1024 exactly)
        let w = st.open(&mut v, 1, b"/p1/shared", OpenModes::WRONLY | OpenModes::CREAT, 0o666, NOW).unwrap();
        st.write(&mut v, 1, w, -1, &[0xAB; 4096], NOW).unwrap();
        st.close(&mut v, 1, w).unwrap();
        let t0 = backend::now_ns();
        let mut fds = Vec::new();
        for _ in 0..1021 {
            fds.push(st.open(&mut v, 1, b"/p1/shared", OpenModes::RDONLY, 0, NOW).unwrap());
        }
        let mut buf = [0u8; 64];
        for &fd in &fds {
            st.read(&mut v, 1, fd, -1, &mut buf, NOW).unwrap();
        }
        let dt = backend::now_ns() - t0;
        ns.push(dt as f64);
        for fd in fds {
            st.close(&mut v, 1, fd).unwrap();
        }
        core::mem::drop((v, st));
    }
    let per = ns.iter().map(|d| d / (1021.0 * 2.0)).collect::<Vec<f64>>();
    let layer = layer_name();
    let gates = [
        Gate { expr: "B-FD-9 1021 opens+reads on one inode <= 10 us/op", value: median(&per), pass: median(&per) <= 10_000.0, note: "independent OFDs, no contention path (serial kernel), median of 5" },
    ];
    reps_fmt(layer, "B-FD9", &mut [("total_ms", &ns.iter().map(|x| x / 1e6).collect())], &gates)
}

// B-FD-10: process overhead — 64 ProcessRecords memory (kalloc live delta).
pub fn bfd10() -> String {
    let mut deltas = Vec::new();
    for _ in 0..REPS {
        let mut v = Vfs::new(NOW);
        let a = backend::alloc_stats();
        let mut st = FdState::new();
        for pid in 1..=64u32 {
            let _ = st.process_birth(&mut v, pid, 0o022);
        }
        let b = backend::alloc_stats();
        deltas.push((b.live_requested - a.live_requested) as f64);
        core::mem::drop((v, st));
    }
    let layer = layer_name();
    let per = median(&deltas) / 64.0;
    let gates = [
        Gate { expr: "B-FD-10 process-record overhead <= 4 KiB/proc", value: per, pass: per <= 4096.0, note: "FdState + 64 records + tables (kalloc live delta, median of 5)" },
    ];
    reps_fmt(layer, "B-FD10", &mut [("total_bytes", &deltas)], &gates)
}

// B-FD-11: concurrent append correctness+throughput (two processes, one log).
pub fn bfd11() -> String {
    let mut rates = Vec::new();
    let mut ok = true;
    for _ in 0..REPS {
        let (mut v, mut st) = fd_boot_procs(2);
        let a = st.open(&mut v, 1, b"/p1/log", OpenModes::WRONLY | OpenModes::CREAT | OpenModes::APPEND, 0o666, NOW).unwrap();
        let b = st.open(&mut v, 2, b"/p1/log", OpenModes::WRONLY | OpenModes::APPEND, 0, NOW).unwrap();
        let chunk = [0x5Au8; 256];
        let t0 = backend::now_ns();
        const N: u64 = 20_000;
        for i in 0..N {
            let fd = if i % 2 == 0 { a } else { b };
            st.write(&mut v, if i % 2 == 0 { 1 } else { 2 }, fd, -1, &chunk, NOW).unwrap();
        }
        let dt = backend::now_ns() - t0;
        let size = st.fstat(&v, 1, a).unwrap().size;
        if size != N * 256 {
            ok = false;
        }
        rates.push((N * 256) as f64 / (dt / 1e9));
        st.close(&mut v, 1, a).unwrap();
        st.close(&mut v, 2, b).unwrap();
        core::mem::drop((v, st));
    }
    let layer = layer_name();
    let gates = [
        Gate { expr: "B-FD-11 interleaved append bytes all land (size == N*256)", value: if ok { 1.0 } else { 0.0 }, pass: ok, note: "2 processes alternating, serial kernel atomic-by-construction" },
        Gate { expr: "B-FD-11 append throughput >= 200 MiB/s", value: median(&rates) / 1048576.0, pass: median(&rates) / 1048576.0 >= 200.0, note: "256-B appends, median of 5" },
    ];
    reps_fmt(layer, "B-FD11", &mut [("bytes_s", &rates)], &gates)
}

// B-FD-12: stdio sink throughput (console.log shape: 120-B lines to fd 1).
pub fn bfd12() -> String {
    let mut rates = Vec::new();
    for _ in 0..REPS {
        let (mut v, mut st) = fd_boot_procs(1);
        let line = [0x41u8; 120];
        let t0 = backend::now_ns();
        const N: u64 = 200_000;
        for _ in 0..N {
            st.write(&mut v, 1, 1, -1, &line, NOW).unwrap();
        }
        let dt = backend::now_ns() - t0;
        rates.push(N as f64 / (dt / 1e9));
        // drain it all back (unmeasured)
        let mut sink = [0u8; 4096];
        while st.stdio_drain(0, &mut sink) > 0 {}
        core::mem::drop((v, st));
    }
    let layer = layer_name();
    let gates = [
        Gate { expr: "B-FD-12 stdio sink >= 5M writes/s (120-B lines)", value: median(&rates), pass: median(&rates) >= 5_000_000.0, note: "console.log shape, bounded FIFO push, median of 5" },
    ];
    reps_fmt(layer, "B-FD12", &mut [("writes_s", &rates)], &gates)
}

fn v_ref(v: &mut Vfs) -> &mut Vfs { v }

/// Small formatter for the FD benches: medians of each series.
fn reps_fmt(layer: &str, id: &str, series: &mut [(&str, &Vec<f64>)], gates: &[Gate]) -> String {
    let mut rep = String::from("{");
    for (i, (name, vals)) in series.iter_mut().enumerate() {
        if i > 0 {
            rep.push(',');
        }
        rep.push_str(&format!("\"{}\":{}", name, json_num(median(vals))));
    }
    rep.push('}');
    emit(id, layer, &[rep], gates, "")
}

/// Run one bench by id (1,2,3,4,5,6,8,9,10 — B7 is the host fixture walk in
/// poc/p2-005). Returns the JSON line.
pub fn run_bench(id: u32) -> Option<String> {
    backend::reset_counters();
    match id {
        0 => Some(b0()),
        1 => Some(b1()),
        2 => Some(b2()),
        3 => Some(b3()),
        4 => Some(b4()),
        5 => Some(b5()),
        6 => Some(b6()),
        8 => Some(b8()),
        9 => Some(b9()),
        10 => Some(b10()),
        // P2-005-005 FD battery (FD_SPEC §12)
        21 => Some(bfd1()),
        22 => Some(bfd2()),
        23 => Some(bfd3()),
        24 => Some(bfd4()),
        25 => Some(bfd5()),
        26 => Some(bfd7()),
        27 => Some(bfd9()),
        28 => Some(bfd10()),
        29 => Some(bfd11()),
        30 => Some(bfd12()),
        _ => None,
    }
}

pub const BENCH_IDS: [u32; 19] = [1, 2, 3, 4, 5, 6, 8, 9, 10, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30];
